#!/usr/bin/env python3
"""amux Cloud control plane: cloud.amux.io (docs/design/cloud-self-service.md, section 3).

Plain code, no agents. Billing, sign-in, provisioning, lifecycle, the redirect
and kill switches all live here and keep working with the Operator stopped.

Env:
  CONTROL_DB, CONTROL_SECRET, CONTROL_PUBLIC_URL, CONTROL_PORT
  STRIPE_SECRET_KEY, STRIPE_WEBHOOK_SECRET
  HCLOUD_TOKEN, CLOUDFLARE_API_TOKEN, CF_ZONE_ID_AMUX_CLOUD, AMUX_GOLDEN_IMAGE
  CLERK_PUBLISHABLE_KEY, CLERK_SECRET_KEY
  CONTROL_ADMIN_TOKEN       kill switches and alerts
  SMTP_URL, CONTROL_MAIL_FROM
  CONTROL_FAKE=1            fake Stripe, Hetzner and Cloudflare, plus a fake checkout
                            page and dev sign-in, for local end-to-end tests only
"""
import html, json, os, sys, threading, time, traceback, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import accounts, apple, deletion, drill, emails, fleet, lifecycle, nudges, provision, support, tokens  # noqa: E402
from price_check import load_plans  # noqa: E402
from store import new_id, open_store  # noqa: E402

PLANS = load_plans()
PLAN_IDS = {p["id"] for p in PLANS["plans"]}
PUBLIC = os.environ.get("CONTROL_PUBLIC_URL", "https://cloud.amux.io").rstrip("/")
SITE = os.environ.get("AMUX_SITE_URL", "https://amux.io").rstrip("/")
CUSTOMER_DOMAIN = os.environ.get("AMUX_CUSTOMER_DOMAIN", "amux.cloud")
SECRET = os.environ.get("CONTROL_SECRET") or ""
FAKE = os.environ.get("CONTROL_FAKE") == "1"
COOKIE = "__Host-amux_cp" if PUBLIC.startswith("https://") else "amux_cp"
SESSION_TTL = 7 * 86400


# ------------------------------------------------------------------ providers
def make_providers():
    if FAKE:
        from fakes import FakeBilling, FakeInfra
        b, i = FakeBilling(), FakeInfra()
        b.sessions = {}
        return b, i
    from billing import StripeBilling
    from infra import Infra
    return (StripeBilling(os.environ["STRIPE_SECRET_KEY"], PLANS),
            Infra(os.environ["HCLOUD_TOKEN"], os.environ["CLOUDFLARE_API_TOKEN"], os.environ["CF_ZONE_ID_AMUX_CLOUD"],
                  CUSTOMER_DOMAIN))


STORE = None
BILLING = INFRA = None


def server_url(srv):
    return f"https://{srv['slug']}.{CUSTOMER_DOMAIN}"


# ------------------------------------------------------------------ pages
STYLE = """<style>
:root{color-scheme:dark light;--fg:#e8e6e1;--bg:#14161a;--dim:#9a988f;--accent:#f0c040}
@media (prefers-color-scheme: light){:root{--fg:#1d1f23;--bg:#f7f6f2;--dim:#55575c;--accent:#7a5a00}}
body{font:16px/1.55 system-ui,sans-serif;background:var(--bg);color:var(--fg);margin:0;padding:max(24px,env(safe-area-inset-top)) 20px}
main{max-width:560px;margin:0 auto}h1{font-size:1.5rem}a{color:var(--accent)}.dim{color:var(--dim)}
.btn{display:inline-flex;align-items:center;min-height:44px;padding:0 18px;border-radius:8px;border:0;
background:var(--accent);color:#14161a;font:inherit;font-weight:600;text-decoration:none;cursor:pointer}
ol.steps li{margin:6px 0}ol.steps li.done{color:var(--dim)}ol.steps li.now{font-weight:600}
input{min-height:44px;padding:0 12px;font:inherit;border-radius:8px;border:1px solid var(--dim);background:transparent;color:inherit}
</style>"""


def page(title, body):
    return (f"<!doctype html><html lang=en><head><meta charset=utf-8>"
            f"<meta name=viewport content='width=device-width,initial-scale=1,viewport-fit=cover'>"
            f"<title>{html.escape(title)} · amux Cloud</title>{STYLE}</head><body><main>{body}</main></body></html>")


PROGRESS_JS = """<script>
const steps=['create','dns','boot','ready'];
async function poll(){
  const r=await fetch('/api/me',{credentials:'same-origin'}); if(!r.ok){location='/';return}
  const me=await r.json(); const s=(me.servers||[])[0]; if(!s){setTimeout(poll,3000);return}
  document.getElementById('host').textContent=s.url;
  const i=Math.max(0,steps.indexOf(s.step));
  document.querySelectorAll('ol.steps li').forEach((li,n)=>{li.className=n<i?'done':(n===i?'now':'')});
  if(s.status==='ready'){location='/go/'+s.id;return}
  if(s.status==='failed'){document.getElementById('msg').textContent='This server could not be built. Your payment is being refunded and we have been told.';return}
  setTimeout(poll,3000)}
poll();
</script>"""


# ------------------------------------------------------------------ handler
class Handler(BaseHTTPRequestHandler):
    server_version = "amux-control"

    def log_message(self, fmt, *a):
        sys.stderr.write("[control] %s %s\n" % (self.address_string(), fmt % a))

    # -- helpers
    def send(self, code, body=b"", ctype="text/html; charset=utf-8", headers=None):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "same-origin")
        for k, v in dict(getattr(self, "extra_headers", None) or {}, **(headers or {})).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def json(self, code, obj, headers=None):
        self.send(code, json.dumps(obj), "application/json", headers)

    def redirect(self, url, headers=None):
        self.send(303, b"", "text/plain", dict({"Location": url}, **(headers or {})))

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(min(n, 2_000_000)) if n else b""

    def query(self):
        return {k: v[0] for k, v in urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query).items()}

    def user(self):
        auth = self.headers.get("Authorization") or ""
        if auth.startswith("Bearer ") and not self.path.startswith("/api/fleet/") and not self.path.startswith("/api/admin/"):
            try:
                claims = tokens.verify(auth[7:].strip(), SECRET)
            except ValueError:
                return None
            if claims.get("kind") != "app":
                return None
            return STORE.one("SELECT * FROM users WHERE id=?", claims["sub"])
        raw = self.headers.get("Cookie") or ""
        for part in raw.split(";"):
            k, _, v = part.strip().partition("=")
            if k == COOKIE:
                try:
                    claims = tokens.verify(v, SECRET)
                except ValueError:
                    return None
                return STORE.one("SELECT * FROM users WHERE id=?", claims["sub"])
        return None

    def session_cookie(self, user_id):
        t = tokens.sign({"sub": user_id}, SECRET, SESSION_TTL)
        secure = "; Secure" if COOKIE.startswith("__Host-") else ""
        return {"Set-Cookie": f"{COOKIE}={t}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL}{secure}"}

    # -- routing
    def do_GET(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def _route(self, method):
        path = urllib.parse.urlparse(self.path).path
        try:
            for m, prefix, fn in ROUTES:
                if m == method and (path == prefix or (prefix != "/" and prefix.endswith("/") and path.startswith(prefix))):
                    return fn(self, path)
            self.send(404, page("Not found", "<h1>Not found</h1>"))
        except Exception as e:
            traceback.print_exc()
            STORE.event("control.error", path=path, error=str(e)[:300])
            self.send(500, page("Something went wrong", "<h1>Something went wrong</h1><p>We have been told.</p>"))


# ------------------------------------------------------------------ routes
def r_health(h, _):
    h.json(200, {"ok": True, "measured": True, "fake": FAKE, "flags": {r["name"]: r["value"] for r in STORE.all("SELECT * FROM flags")},
                 "email": "configured" if emails.configured() else "not configured: mail is queued, not sent",
                 "prices": PLANS["status"],
                 "n_considered": STORE.one("SELECT COUNT(*) n FROM servers")["n"]})


def r_root(h, _):
    """The redirect table (plan 8.1): every visitor goes to exactly one place."""
    q = h.query()
    u = h.user()
    if not u:
        if q.get("plan") in PLAN_IDS:
            return h.redirect(f"/start?plan={q['plan']}&source={urllib.parse.quote(q.get('source', 'cloud'))}")
        if q.get("server"):
            return h.redirect("/signin")
        if q.get("from") == "ios" or q.get("signin"):
            return h.redirect("/signin" + ("?from=ios" if q.get("from") == "ios" else ""))
        return h.redirect(f"{SITE}/pricing/")
    servers = visible_servers(u)
    live = [s for s in servers if s["status"] in ("ready", "provisioning")]
    if q.get("server"):                                    # sent back by a server's door when its cookie expired
        match = next((s for s in live if s["slug"] == q["server"] and s["status"] == "ready"), None)
        if match:
            return h.redirect(f"/go/{match['id']}")
    if not live:
        snap = STORE.one("SELECT a.* FROM lifecycle_actions a JOIN servers s ON s.id=a.server_id WHERE s.org_id=? "
                         "AND a.state='done' ORDER BY a.decided_at DESC LIMIT 1", u["org_id"])
        if snap:
            return h.send(200, page("Restore", f"<h1>Your server was saved and turned off</h1><p>Restore it from its snapshot "
                                    f"by resubscribing.</p><p><a class=btn href='/billing'>Restore my server</a></p>"))
        return h.redirect(f"{SITE}/pricing/")
    if len(live) > 1 and not q.get("server"):
        items = "".join(f"<li><a href='/go/{s['id']}'>{html.escape(server_url(s))}</a> <span class=dim>{s['plan']}, "
                        f"{s['status']}</span></li>" for s in live)
        return h.send(200, page("Your servers", f"<h1>Your servers</h1><ul>{items}</ul>"))
    srv = live[0]
    if srv["status"] == "provisioning":
        return h.redirect("/progress")
    h.redirect(f"/go/{srv['id']}")


def visible_servers(u):
    """Servers this person may enter: their own org's (owner) plus any they joined by invite."""
    own = STORE.all("SELECT *, 'owner' AS my_role FROM servers WHERE org_id=? ORDER BY created_at DESC", u["org_id"]) \
        if u["role"] == "owner" else []
    joined = STORE.all("SELECT s.*, 'member' AS my_role FROM servers s JOIN memberships m ON m.server_id=s.id "
                       "WHERE m.user_id=? ORDER BY s.created_at DESC", u["id"])
    seen, out = set(), []
    for srv in own + joined:
        if srv["id"] not in seen:
            seen.add(srv["id"])
            out.append(srv)
    return out


def r_start(h, _):
    q = h.query()
    plan = q.get("plan") if q.get("plan") in PLAN_IDS else "solo"
    if q.get("plan") and q.get("plan") not in PLAN_IDS:
        STORE.event("cloud.unknown_plan", plan=q.get("plan"))
    if STORE.flag("provisioning_freeze") == "1":
        return h.send(503, page("Back soon", "<h1>Back soon</h1><p>New servers are paused for a moment. Existing "
                                "servers are unaffected. Please try again shortly.</p>"))
    if PLANS["status"] != "approved" and not FAKE:
        return h.redirect(f"{SITE}/cloud/#notify")
    signup = new_id("su")
    u = h.user()
    org = STORE.one("SELECT * FROM orgs WHERE id=?", u["org_id"]) if u else None
    sess = BILLING.create_checkout(plan, signup, q.get("source") or "direct",
                                   f"{PUBLIC}/welcome?session_id={{CHECKOUT_SESSION_ID}}", f"{SITE}/pricing/",
                                   customer_email=None if org else q.get("email"),
                                   customer_id=org["stripe_customer_id"] if org else None)
    STORE.event("cloud.checkout_started", org["id"] if org else None, plan=plan, source=q.get("source"))
    h.redirect(sess["url"])


def r_welcome(h, _):
    sid = h.query().get("session_id") or ""
    try:
        r = accounts.complete_checkout(STORE, BILLING, sid)
    except accounts.NotPaid:
        return h.send(402, page("Payment not complete", "<h1>Payment not complete</h1><p>Your checkout did not finish. "
                                f"<a href='{SITE}/pricing/'>Back to pricing</a></p>"))
    # One-time sign-in for the browser that paid (J5); other devices sign in through Clerk.
    h.redirect("/progress", h.session_cookie(r["user"]["id"]))


def r_progress(h, _):
    if not h.user():
        return h.redirect("/signin")
    h.send(200, page("Building your server", "<h1>Building your server</h1><p class=dim id=host></p><ol class=steps>"
                     "<li>Creating your server</li><li>Setting up its address</li><li>Starting amux and taking the "
                     "first backup</li><li>Ready</li></ol><p id=msg class=dim>Usually about three minutes. While you "
                     "wait: have an Anthropic or OpenAI key ready, or your Claude, ChatGPT or Google sign-in.</p>" + PROGRESS_JS))


def r_me(h, _):
    u = h.user()
    if not u:
        return h.json(401, {"error": "signed out"})
    out = []
    steps = {r["server_id"]: r["step"] for r in STORE.all("SELECT server_id, step FROM provision_jobs")}
    for s in [dict(x, step=steps.get(x["id"])) for x in visible_servers(u)]:
        out.append({"id": s["id"], "slug": s["slug"], "url": server_url(s), "status": s["status"], "plan": s["plan"],
                    "role": s["my_role"],
                    "step": "ready" if s["status"] == "ready" else s["step"]})
    h.json(200, {"email": u["email"], "email_verified": bool(u["email_verified"]), "servers": out})


def r_go(h, path):
    u = h.user()
    if not u:
        return h.redirect("/signin")
    sid = path.rsplit("/", 1)[-1]
    srv = next((s for s in visible_servers(u) if s["id"] == sid and s["status"] == "ready"), None)
    if not srv:
        return h.redirect("/")
    STORE.event("cloud.enter", srv["org_id"], server=srv["id"], role=srv["my_role"])
    h.redirect(f"{server_url(srv)}/_door/enter?t={tokens.door_token(srv, u['email'], role=srv['my_role'])}")


def r_webhook(h, _):
    from billing import verify_webhook
    payload = h.body()
    if FAKE:
        event = json.loads(payload)
    else:
        try:
            event = verify_webhook(payload, h.headers.get("Stripe-Signature"), os.environ["STRIPE_WEBHOOK_SECRET"])
        except ValueError as e:
            STORE.event("stripe.webhook_rejected", reason=str(e))
            return h.json(400, {"error": str(e)})
    h.json(200, {"handled": accounts.handle_event(
        STORE, BILLING, event, on_plan_change=lambda sub, plan: nudges.upgrade_if_plan_changed(STORE, INFRA, PLANS, sub, plan))})


def _fleet_server(h, path):
    parts = path.strip("/").split("/")          # api/fleet/<server_id>/<verb>
    auth = (h.headers.get("Authorization") or "").removeprefix("Bearer ").strip()
    return fleet.authenticate(STORE, parts[2], auth) if len(parts) == 4 else None, parts[-1]


def r_fleet(h, path):
    srv, verb = _fleet_server(h, path)
    if not srv:
        return h.json(401, {"error": "unknown server or bad fleet token"})
    body = json.loads(h.body() or b"{}")
    if verb == "heartbeat":
        return h.json(200, fleet.heartbeat(STORE, INFRA, srv, body))
    if verb == "invite-email":
        code, out = fleet.invite_email(STORE, srv, body)
        return h.json(code, out)
    h.json(404, {"error": f"unknown fleet verb {verb}"})


def r_notify(h, _):
    """Waitlist (plan step 0). CORS-open to amux.io so the site form can post here once this is live."""
    cors = {"Access-Control-Allow-Origin": SITE}
    try:
        body = json.loads(h.body() or b"{}")
    except ValueError:
        return h.json(400, {"error": "bad json"}, cors)
    email = (body.get("email") or "").strip().lower()
    if "@" not in email or len(email) > 200:
        return h.json(400, {"error": "email required"}, cors)
    STORE.run("INSERT OR IGNORE INTO waitlist(email,source,created_at) VALUES(?,?,?)", email,
              (body.get("source") or "site")[:60], time.time())
    STORE.event("cloud.notify_signup", source=body.get("source"))
    h.json(200, {"ok": True}, cors)


def r_notify_options(h, _):
    h.send(204, b"", "text/plain", {"Access-Control-Allow-Origin": SITE, "Access-Control-Allow-Methods": "POST",
                                    "Access-Control-Allow-Headers": "Content-Type"})


APP_TTL = 30 * 86400
IOS_COOKIE = "amux_ios"


def app_token(user_id):
    """The iOS app's bearer: same signature as the web session, marked so a cookie can never pass for it."""
    return tokens.sign({"sub": user_id, "kind": "app"}, SECRET, APP_TTL)


def r_apple_signin(h, _):
    """8.3: verify Apple's token, find the linked account, never create one."""
    try:
        body = json.loads(h.body() or b"{}")
        who = apple.verify_identity_token(body.get("identity_token") or "")
    except (ValueError, json.JSONDecodeError) as e:
        STORE.event("cloud.apple_refused", reason=str(e)[:200])
        return h.json(401, {"error": "Apple sign-in could not be verified"})
    u = STORE.one("SELECT * FROM users WHERE apple_sub=?", who["sub"])
    if not u and who["email"] and who["email_verified"] and not who["private_relay"]:
        u = STORE.one("SELECT * FROM users WHERE lower(email)=?", who["email"])
        if u:
            STORE.run("UPDATE users SET apple_sub=? WHERE id=?", who["sub"], u["id"])
    if not u:
        link = tokens.sign({"apple_sub": who["sub"]}, SECRET, 900)
        return h.json(404, {"link_required": True,
                            "message": "No amux Cloud server for this Apple ID. Sign in with your email once to link it.",
                            "link_url": f"{PUBLIC}/signin?from=ios&link={urllib.parse.quote(link)}"})
    code = body.get("authorization_code")
    if code and apple.configured():
        try:
            rt = apple.exchange_code(code)
            if rt:
                STORE.run("UPDATE users SET apple_refresh=? WHERE id=?", rt, u["id"])
        except Exception as e:
            STORE.event("cloud.apple_exchange_failed", u["org_id"], error=str(e)[:200])
    STORE.event("cloud.signin", u["org_id"], method="apple")
    h.json(200, {"token": app_token(u["id"]), "email": u["email"]})


def r_go_url(h, path):
    """The app asks for its door link as JSON, so no token ever sits in a URL the app builds itself."""
    u = h.user()
    if not u:
        return h.json(401, {"error": "signed out"})
    sid = path.rsplit("/", 1)[-1]
    srv = next((s for s in visible_servers(u) if s["id"] == sid and s["status"] == "ready"), None)
    if not srv:
        return h.json(404, {"error": "no such server"})
    STORE.event("cloud.enter", srv["org_id"], server=srv["id"], role=srv["my_role"], via="app")
    h.json(200, {"url": f"{server_url(srv)}/_door/enter?t={tokens.door_token(srv, u['email'], role=srv['my_role'])}"})


def r_account_delete(h, _):
    """J18/M19: the account holder asks; every step is recorded and retried until done."""
    u = h.user()
    if not u:
        return h.json(401, {"error": "signed out"})
    body = json.loads(h.body() or b"{}") if (h.headers.get("Content-Type") or "").startswith("application/json") else \
        {k: v[0] for k, v in urllib.parse.parse_qs(h.body().decode()).items()}
    if (body.get("confirm") or "").strip().lower() != "delete":
        return h.json(400, {"error": "type delete to confirm"})
    did = deletion.request(STORE, u)
    clear = {"Set-Cookie": f"{COOKIE}=; Path=/; Max-Age=0" + ("; Secure" if COOKIE.startswith("__Host-") else "")}
    h.json(202, {"deletion": did, "message": "Deletion started. A receipt listing every item is emailed when it finishes."}, clear)


DEVICE_TTL = 600
DEVICE_ALPHABET = "BCDFGHJKLMNPQRSTVWXZ23456789"   # no vowels, no 0/1/O/I: nothing to misread or spell


def r_device_start(h, _):
    """`amux cloud login`: a device code the person approves at /device while signed in (plan 8.4)."""
    import secrets as _s
    dc = _s.token_urlsafe(32)
    uc = "".join(_s.choice(DEVICE_ALPHABET) for _ in range(4)) + "-" + "".join(_s.choice(DEVICE_ALPHABET) for _ in range(4))
    now = time.time()
    STORE.run("INSERT INTO device_codes(device_code,user_code,created_at,expires_at) VALUES(?,?,?,?)", dc, uc, now, now + DEVICE_TTL)
    h.json(200, {"device_code": dc, "user_code": uc, "verify_url": f"{PUBLIC}/device?code={uc}",
                 "expires_in": DEVICE_TTL, "interval": 3})


def r_device_poll(h, _):
    body = json.loads(h.body() or b"{}")
    row = STORE.one("SELECT * FROM device_codes WHERE device_code=?", body.get("device_code") or "")
    if not row or row["expires_at"] < time.time():
        return h.json(410, {"error": "expired"})
    if row["state"] == "denied":
        return h.json(403, {"error": "denied"})
    if row["state"] == "used":
        return h.json(410, {"error": "this sign-in was already used"})
    if row["state"] != "approved":
        return h.json(202, {"pending": True})
    STORE.run("UPDATE device_codes SET state='used' WHERE device_code=?", row["device_code"])   # one token per approval
    u = STORE.one("SELECT * FROM users WHERE id=?", row["user_id"])
    STORE.event("cloud.signin", u["org_id"], method="cli")
    h.json(200, {"token": app_token(u["id"]), "email": u["email"]})


def r_device_page(h, _):
    u = h.user()
    if not u:
        return h.redirect("/signin")
    code = (h.query().get("code") or "").upper()
    if h.command == "POST":
        form = {k: v[0] for k, v in urllib.parse.parse_qs(h.body().decode()).items()}
        code = (form.get("code") or "").strip().upper()
        row = STORE.one("SELECT * FROM device_codes WHERE user_code=? AND state='pending' AND expires_at>?", code, time.time())
        if not row:
            return h.send(400, page("Sign in a CLI", "<h1>That code is wrong or expired</h1><p>Run <code>amux cloud login</code> again.</p>"))
        state = "approved" if form.get("decision") == "approve" else "denied"
        STORE.run("UPDATE device_codes SET state=?, user_id=? WHERE device_code=?", state, u["id"], row["device_code"])
        return h.send(200, page("Sign in a CLI", f"<h1>{'Signed in' if state == 'approved' else 'Refused'}</h1>"
                                "<p>You can close this tab and go back to your terminal.</p>"))
    h.send(200, page("Sign in a CLI", f"<h1>Sign in the amux CLI</h1><p>Only continue if you just ran "
                     f"<code>amux cloud login</code> yourself. Signing in as {html.escape(u['email'])}.</p>"
                     f"<form method=post><input name=code value='{html.escape(code)}' placeholder='ABCD-EFGH' required> "
                     "<button class=btn name=decision value=approve>Sign in</button> "
                     "<button name=decision value=deny>Cancel</button></form>"))


INVITE_COOKIE = "amux_invite"


def r_invite(h, path):
    """8.2: cloud.amux.io/invite/<slug>/<invite>. Sign in first; the door redeems the invite."""
    parts = path.strip("/").split("/")
    if len(parts) != 3:
        return h.send(404, page("Invite", "<h1>This invite link is incomplete</h1>"))
    _, slug, invite = parts
    srv = STORE.one("SELECT * FROM servers WHERE slug=? AND status='ready'", slug)
    if not srv:
        return h.send(404, page("Invite", "<h1>This invite is for a server that is not running</h1>"
                                "<p>Ask the person who invited you for a new link.</p>"))
    u = h.user()
    if not u:
        pending = f"{INVITE_COOKIE}={slug}/{invite}; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600" + \
                  ("; Secure" if PUBLIC.startswith("https://") else "")
        return h.redirect("/signin", {"Set-Cookie": pending})
    STORE.run("INSERT OR IGNORE INTO memberships(user_id,server_id,created_at) VALUES(?,?,?)", u["id"], srv["id"], time.time())
    STORE.event("cloud.invite_followed", srv["org_id"], server=srv["id"])
    # The door refuses the invite if this email is not the one it was sent to (T-TEAM-4).
    h.redirect(f"{server_url(srv)}/_door/enter?t={tokens.door_token(srv, u['email'], role='member', invite=invite)}",
               {"Set-Cookie": f"{INVITE_COOKIE}=; Path=/; Max-Age=0"})


def _cookie(h, name):
    for part in (h.headers.get("Cookie") or "").split(";"):
        k, _, v = part.strip().partition("=")
        if k == name:
            return v
    return None


def pending_invite(h):
    for part in (h.headers.get("Cookie") or "").split(";"):
        k, _, v = part.strip().partition("=")
        if k == INVITE_COOKIE and "/" in v:
            return v.split("/", 1)
    return None


def r_signin(h, _):
    q = h.query()
    if q.get("from") == "ios":
        # Remember for r_session: send the app its token at amux://signed-in, and link Apple if asked.
        v = "1" + (("." + q["link"]) if q.get("link") else "")
        h.extra_headers = {"Set-Cookie": f"{IOS_COOKIE}={urllib.parse.quote(v)}; Path=/; HttpOnly; SameSite=Lax; Max-Age=900"
                                         + ("; Secure" if PUBLIC.startswith("https://") else "")}
    if FAKE:
        return h.send(200, page("Sign in", "<h1>Sign in (fake mode)</h1><form method=post action=/api/session>"
                                "<input name=email type=email required placeholder='you@example.com'> "
                                "<button class=btn>Sign in</button></form>"))
    pk = os.environ.get("CLERK_PUBLISHABLE_KEY", "")
    h.send(200, page("Sign in", f"""<h1>Sign in to amux Cloud</h1><div id=clerk></div>
<script async crossorigin data-clerk-publishable-key="{html.escape(pk)}"
 src="https://cdn.jsdelivr.net/npm/@clerk/clerk-js@5/dist/clerk.browser.js" onload="go()"></script>
<script>async function go(){{await window.Clerk.load();
 if(window.Clerk.user){{const t=await window.Clerk.session.getToken();
  const r=await fetch('/api/session',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{token:t}})}});
  const j=await r.json().catch(()=>({{}}));location=r.ok?(j.next||'/'):'/signin?error=1';return}}
 window.Clerk.mountSignIn(document.getElementById('clerk'),{{afterSignInUrl:'/signin',afterSignUpUrl:'/signin'}})}}</script>"""))


def r_session(h, _):
    raw = h.body()
    if FAKE:
        form = urllib.parse.parse_qs(raw.decode())
        email = (form.get("email") or [""])[0].strip().lower()
        verified = True
    else:
        from clerk import verify_session
        try:
            email, verified = verify_session(json.loads(raw)["token"])
        except Exception as e:
            STORE.event("control.signin_rejected", reason=str(e)[:200])
            return h.json(401, {"error": "sign-in could not be verified"})
    u = STORE.one("SELECT * FROM users WHERE lower(email)=?", email)
    inv = pending_invite(h)
    if not u and inv and verified:
        srv = STORE.one("SELECT * FROM servers WHERE slug=? AND status='ready'", inv[0])
        if srv:
            uid = new_id("usr")
            STORE.run("INSERT INTO users(id,email,email_verified,org_id,created_at,role) VALUES(?,?,1,?,?,'member')",
                      uid, email, srv["org_id"], time.time())
            STORE.event("cloud.member_joined", srv["org_id"], server=srv["id"])
            u = STORE.one("SELECT * FROM users WHERE id=?", uid)
    ios = _cookie(h, IOS_COOKIE)
    if u and ios:
        flag, _, link = urllib.parse.unquote(ios).partition(".")
        if link:
            try:
                sub = tokens.verify(link, SECRET)["apple_sub"]
                STORE.run("UPDATE users SET apple_sub=? WHERE id=? AND (apple_sub IS NULL OR apple_sub=?)", sub, u["id"], sub)
                STORE.event("cloud.apple_linked", u["org_id"])
            except ValueError:
                STORE.event("cloud.apple_link_expired", u["org_id"])
        target = f"amux://signed-in?token={urllib.parse.quote(app_token(u['id']))}"
        clear = {"Set-Cookie": f"{IOS_COOKIE}=; Path=/; Max-Age=0"}
        return h.redirect(target, clear) if FAKE else h.json(200, {"ok": True, "next": target}, clear)
    if u and inv:
        cookie = h.session_cookie(u["id"])
        target = f"/invite/{inv[0]}/{inv[1]}"
        return h.redirect(target, cookie) if FAKE else h.json(200, {"ok": True, "next": target}, cookie)
    if not u:
        # Signing in creates nothing: an account comes from paying (D6) or an invite.
        return (h.redirect(f"{SITE}/pricing/?no_account=1") if FAKE else h.json(404, {"error": "no account", "pricing": f"{SITE}/pricing/"}))
    if verified and not u["email_verified"]:
        STORE.run("UPDATE users SET email_verified=1 WHERE id=?", u["id"])
    cookie = h.session_cookie(u["id"])
    return h.redirect("/", cookie) if FAKE else h.json(200, {"ok": True}, cookie)


def r_signout(h, _):
    h.redirect(f"{SITE}/", {"Set-Cookie": f"{COOKIE}=; Path=/; Max-Age=0" + ("; Secure" if COOKIE.startswith("__Host-") else "")})


def r_network(h, path):
    """Plan 8.5: the owner's tailnet-only switch lives here, so a customer whose tailnet ACLs lock
    them out can always turn it off from cloud.amux.io (the fleet agent applies it outbound)."""
    u = h.user()
    if not u:
        return h.redirect("/signin")
    sid = path.rstrip("/").split("/")[-2] if path.endswith("/network") else ""
    srv = next((s for s in visible_servers(u) if s["id"] == sid and s["my_role"] == "owner"), None)
    if not srv:
        return h.send(404, page("Network", "<h1>Not your server</h1>"))
    if h.command == "POST":
        form = urllib.parse.parse_qs(h.body().decode())
        want = 1 if form.get("tailnet_only", ["0"])[0] == "1" else 0
        if want and not srv["tailnet_url"]:
            return h.send(409, page("Network", "<h1>Add the server to your tailnet first</h1><p>Tailnet only would leave "
                                    "you no way in. Open Settings, then Your tailnet, on the server.</p>"))
        STORE.run("UPDATE servers SET tailnet_only=? WHERE id=?", want, srv["id"])
        STORE.event("cloud.tailnet_only", srv["org_id"], server=srv["id"], on=bool(want))
        return h.redirect(f"/servers/{srv['id']}/network")
    on = bool(srv["tailnet_only"])
    tn = html.escape(srv["tailnet_url"] or "")
    body = (f"<h1>{html.escape(server_url(srv))}</h1><p>Tailnet address: " + (f"<a href='{tn}'>{tn}</a>" if tn else
            "<span class=dim>not on a tailnet yet (Settings, then Your tailnet, on the server)</span>") + "</p>"
            f"<p>Public address is <strong>{'off' if on else 'on'}</strong>.</p>"
            f"<form method=post><input type=hidden name=tailnet_only value={'0' if on else '1'}>"
            f"<button class=btn>{'Turn the public address back on' if on else 'Tailnet only: turn the public address off'}</button></form>"
            "<p class=dim>Changes reach the server within a minute. Backups, updates and this page keep working either way.</p>")
    h.send(200, page("Network", body))


def r_help(h, _):
    """J19: "Something wrong?" from cloud.amux.io."""
    u = h.user()
    if not u:
        return h.redirect("/signin")
    servers = [s for s in visible_servers(u) if s["status"] in ("ready", "provisioning")]
    if h.command == "POST":
        form = {k: v[0] for k, v in urllib.parse.parse_qs(h.body().decode()).items()}
        srv = next((s for s in servers if s["id"] == form.get("server")), servers[0] if servers else None)
        try:
            support.open_request(STORE, u, form.get("message"), srv)
        except ValueError as e:
            return h.send(400, page("Help", f"<h1>Help</h1><p>{html.escape(str(e))}</p>"))
        return h.send(200, page("Help", "<h1>Thanks</h1><p>We have your message and a snapshot of your server's health "
                                "(no files or messages). You will get an answer by email.</p>"))
    opts = "".join(f"<option value='{s['id']}'>{html.escape(s['slug'])}</option>" for s in servers)
    h.send(200, page("Help", "<h1>Something wrong?</h1><form method=post>" +
                     (f"<p><select name=server>{opts}</select></p>" if opts else "") +
                     "<p><textarea name=message rows=6 style='width:100%' required placeholder='What happened, and what did you expect?'>"
                     "</textarea></p><button class=btn>Send</button></form>"
                     "<p class=dim>We attach your server's health, version, disk and backup age. Never your files or messages.</p>"))


def r_billing(h, _):
    u = h.user()
    if not u:
        return h.redirect("/signin")
    if u["role"] != "owner":
        return h.send(403, page("Billing", "<h1>Billing</h1><p>Only the account owner can change billing.</p>"))
    org = STORE.one("SELECT * FROM orgs WHERE id=?", u["org_id"])
    if FAKE or not org["stripe_customer_id"]:
        return h.send(200, page("Billing", "<h1>Billing</h1><p class=dim>No billing portal in this mode.</p>"))
    h.redirect(BILLING.portal_url(org["stripe_customer_id"], f"{PUBLIC}/"))


def r_admin(h, path):
    """Kill switches (plan 11.2) and alerts, behind CONTROL_ADMIN_TOKEN."""
    tok = os.environ.get("CONTROL_ADMIN_TOKEN")
    if not tok or (h.headers.get("Authorization") or "") != f"Bearer {tok}":
        return h.json(401, {"error": "admin token required"})
    if path == "/api/admin/flags" and h.command == "POST":
        body = json.loads(h.body() or b"{}")
        for k, v in body.items():
            if k not in ("lifecycle_freeze", "deploy_freeze", "provisioning_freeze", "spend_freeze", "lifecycle_shadow",
                         "support_auto_reply"):
                return h.json(400, {"error": f"unknown flag {k}"})
            STORE.set_flag(k, "1" if v in (1, "1", True, "true") else "0")
            STORE.event("control.flag", flag=k, value=str(v))
    if path == "/api/admin/synthetic" and h.command == "POST":
        run = json.loads(h.body() or b"{}")
        STORE.event("synthetic.run", ok=bool(run.get("ok")), fake=bool(run.get("fake")), ms=run.get("ms"),
                    failed=[s["name"] for s in run.get("steps", []) if not s.get("ok")])
        if not run.get("ok"):
            last = STORE.all("SELECT props FROM events WHERE name='synthetic.run' ORDER BY ts DESC LIMIT 2")
            if len(last) == 2 and not any(json.loads(r["props"]).get("ok") for r in last):
                STORE.alert("synthetic_failing", "the synthetic customer failed twice in a row: " +
                            ", ".join(s["name"] for s in run.get("steps", []) if not s.get("ok")))
        return h.json(200, {"recorded": True})
    if path == "/api/admin/support":
        return h.json(200, {"measured": True, "open": support.queue(STORE)})
    if path == "/api/admin/support/draft" and h.command == "POST":
        b = json.loads(h.body() or b"{}")
        try:
            return h.json(200, {"result": support.draft(STORE, b.get("id"), b.get("text") or "")})
        except ValueError as e:
            return h.json(400, {"error": str(e)})
    if path == "/api/admin/support/approve" and h.command == "POST":
        b = json.loads(h.body() or b"{}")
        try:
            return h.json(200, {"result": support.send(STORE, b.get("id"))})
        except ValueError as e:
            return h.json(400, {"error": str(e)})
    if path == "/api/admin/report":
        import report
        lines, waiting = report.build(STORE, days=int(h.query().get("days", "7")))
        return h.json(200, {"measured": True, "lines": lines, "waiting": waiting,
                            "text": report.render(lines, waiting, int(h.query().get("days", "7")))})
    h.json(200, {"flags": {r["name"]: r["value"] for r in STORE.all("SELECT * FROM flags")},
                 "alerts": STORE.all("SELECT * FROM alerts WHERE acknowledged=0 ORDER BY id DESC LIMIT 50")})


# fake mode only: a stand-in for Stripe Checkout so the journey runs end to end locally
def r_fake_checkout(h, path):
    if not FAKE:
        return h.send(404, "")
    sid = path.rsplit("/", 1)[-1]
    sess = BILLING.sessions.get(sid)
    if not sess:
        return h.send(404, "")
    if h.command == "POST":
        email = urllib.parse.parse_qs(h.body().decode()).get("email", ["buyer@example.com"])[0]
        sub = "sub_" + sid[3:]
        sess.update(status="complete", payment_status="paid", subscription=sub, customer="cus_" + sid[3:],
                    customer_details={"email": email}, amount_total=sess["amount"], payment_intent="pi_" + sid[3:])
        BILLING.subs[sub] = {"id": sub, "status": "active", "current_period_end": time.time() + 30 * 86400}
        return h.redirect(sess["success_url"].replace("{CHECKOUT_SESSION_ID}", sid))
    plan = sess["metadata"]["plan"]
    h.send(200, page("Fake checkout", f"<h1>Fake checkout</h1><p>{plan}: ${sess['amount'] // 100}/month</p><form method=post>"
                     "<input name=email type=email required placeholder=email> <button class=btn id=pay>Pay</button></form>"))


ROUTES = [
    ("GET", "/health", r_health), ("GET", "/", r_root), ("GET", "/start", r_start), ("GET", "/welcome", r_welcome),
    ("GET", "/progress", r_progress), ("GET", "/api/me", r_me), ("GET", "/go/", r_go),
    ("POST", "/webhooks/stripe", r_webhook), ("POST", "/api/fleet/", r_fleet),
    ("POST", "/api/notify", r_notify), ("OPTIONS", "/api/notify", r_notify_options),
    ("GET", "/invite/", r_invite), ("POST", "/api/apple/signin", r_apple_signin), ("GET", "/api/go-url/", r_go_url),
    ("POST", "/api/account/delete", r_account_delete),
    ("POST", "/api/device/start", r_device_start), ("POST", "/api/device/poll", r_device_poll),
    ("GET", "/device", r_device_page), ("POST", "/device", r_device_page), ("GET", "/signin", r_signin), ("POST", "/api/session", r_session), ("GET", "/signout", r_signout),
    ("GET", "/billing", r_billing), ("GET", "/help", r_help), ("POST", "/help", r_help), ("GET", "/servers/", r_network), ("POST", "/servers/", r_network), ("GET", "/api/admin/", r_admin), ("POST", "/api/admin/", r_admin),
    ("GET", "/fake/checkout/", r_fake_checkout), ("POST", "/fake/checkout/", r_fake_checkout),
]
Handler.do_OPTIONS = lambda self: self._route("OPTIONS")


def install_fake_checkout(billing):
    def create_checkout(plan_id, signup_id, source, success_url, cancel_url, customer_email=None, customer_id=None):
        sid = "cs_" + signup_id[3:]
        amount = next(p["price_usd"] for p in PLANS["plans"] if p["id"] == plan_id) * 100
        billing.sessions[sid] = {"id": sid, "status": "open", "payment_status": "unpaid", "mode": "subscription",
                                 "metadata": {"plan": plan_id, "source": source}, "amount": amount,
                                 "success_url": success_url, "url": f"{PUBLIC}/fake/checkout/{sid}"}
        return billing.sessions[sid]
    billing.create_checkout = create_checkout


# ------------------------------------------------------------------ background loops
def loop(name, every, fn):
    def run():
        while True:
            try:
                fn()
            except Exception as e:
                traceback.print_exc()
                STORE.event("control.loop_error", loop=name, error=str(e)[:300])
            time.sleep(every)
    threading.Thread(target=run, name=name, daemon=True).start()


def weekly_report_tick(now=None):
    """Monday 08:00 UTC: queue the owner's report (plan section 9). Deduped per week, so restarts never double-send."""
    owner = os.environ.get("CONTROL_OWNER_EMAIL")
    now = now or time.time()
    t = time.gmtime(now)
    if not owner or t.tm_wday != 0 or t.tm_hour != 8:
        return False
    import report
    lines, waiting = report.build(STORE, now=now)
    STORE.enqueue_email(owner, "weekly_report", dedupe=f"weekly:{time.strftime('%G-%V', t)}",
                        text=report.render(lines, waiting))
    return True


def main():
    global STORE, BILLING, INFRA
    if not SECRET and not FAKE:
        sys.exit("CONTROL_SECRET is required")
    STORE = open_store()
    support.ensure(STORE)
    BILLING, INFRA = make_providers()
    if FAKE:
        globals()["SECRET"] = SECRET or "fake-secret"
        install_fake_checkout(BILLING)
    golden = os.environ.get("AMUX_GOLDEN_IMAGE", "debian-12")
    refund = accounts.provisioning_refund(BILLING)
    loop("provision", 5 if FAKE else 15, lambda: provision.tick(STORE, INFRA, PLANS, golden, PUBLIC, refund=refund))
    loop("lifecycle", 3600, lambda: lifecycle.sweep(STORE, BILLING, INFRA))
    loop("restore-drill", 3600, lambda: drill.sweep(STORE, INFRA, golden, PUBLIC))
    loop("weekly-report", 3600, weekly_report_tick)
    loop("nudges", 3600, lambda: nudges.sweep(STORE))
    loop("resize", 30, lambda: nudges.resize_tick(STORE, INFRA, PLANS))
    loop("price-guard", 86400, lambda: nudges.price_guard(STORE, PLANS, INFRA.pricing()))
    loop("refunds", 3600, lambda: accounts.settle_queued_refunds(STORE, BILLING))
    loop("email", 30, lambda: emails.drain(STORE))
    loop("deletions", 60, lambda: deletion.run(STORE, BILLING, INFRA,
                                              revoke_apple=apple.revoke if apple.configured() else None))
    port = int(os.environ.get("CONTROL_PORT", "8090"))
    print(f"[control] listening on :{port} public={PUBLIC} fake={FAKE} prices={PLANS['status']} "
          f"email={'configured' if emails.configured() else 'NOT configured'}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
