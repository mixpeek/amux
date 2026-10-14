#!/usr/bin/env python3
"""amux-door: the sign-in in front of a dedicated amux host.

Caddy asks this service about every request (`forward_auth`). A request passes
with either a signed `amux_door` cookie (browsers, after the sign-in form) or
HTTP Basic credentials (the amux CLI, remote.py, scripts). Either way the
answer carries `X-Door-User: <username>`, which Caddy maps to that person's
amux member cookie, so the amux identity is the account that signed in.

Why a form at all: a browser can carry Basic auth only through its own modal,
and a URL with embedded credentials breaks the dashboard (Chromium and WebKit
both refuse every fetch from such a page; tested 2026-10-06). The form lets
another amux's Settings > Connect > Connections sign in with a stored
username and password by posting here, top level, then landing on the
dashboard.

Stdlib only. Users live in USERS_FILE as `user scrypt$<salt>$<hash>` lines,
written by `door.py hash`. The cookie is `user.expiry.hmac` under a random
per-host secret; rotating the secret signs everyone out.

Token mode (amux Cloud, docs/design/cloud-self-service.md J8, 8.2): when
/etc/amux/fleet.env holds AMUX_FLEET_SECRET and AMUX_SLUG, /_door/enter?t=...
accepts a 60-second token signed by cloud.amux.io with this server's secret.
The token names the person (sub = email), their role, and for a teammate the
amux invite to redeem. The door redeems it with amux-server, keeps the member
token in MEMBERS_FILE, and sets a 12-hour cookie.

Every account, password or token, maps to an amux member token in MEMBERS_FILE
("name token" lines). /_door/check returns it as X-Amux-Member, which Caddy
forwards as the only cookie amux sees. Each token is re-validated against
amux-server's /api/identity at most every 60 seconds, so revoking a member in
Team settings locks them out within a minute.

  door.py serve                 listen on 127.0.0.1:8890
  door.py hash <user>           read a password on stdin, print a users line
"""
import base64
import hashlib
import hmac
import html
import json
import os
import secrets
import sys
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

USERS_FILE = os.environ.get("DOOR_USERS", "/etc/amux-door/users")
SECRET_FILE = os.environ.get("DOOR_SECRET", "/etc/amux-door/secret")
COOKIE = "amux_door"
# File previews run in a sandboxed iframe (an opaque origin), so their link
# clicks are cross-site and never carry a SameSite=Lax cookie. This second,
# read-only cookie is SameSite=None and valid ONLY for GET under VIEW_PATH,
# the path-style file route the dashboard points previews at.
VIEW_COOKIE = "amux_door_view"
VIEW_PATH = "/api/file/raw-path/"
# The view cookie is scoped to VIEW_PATH, so it is never sent for "/". This
# Path=/ marker records that it was issued; without it the refresh hop below
# would fire on every page load.
VIEW_MARK = "amux_door_vm"
MAX_AGE = 30 * 86400
TOKEN_MAX_AGE = 12 * 3600
PORT = int(os.environ.get("DOOR_PORT", "8890"))
MEMBERS_FILE = os.environ.get("DOOR_MEMBERS", "/etc/amux-door/members")
FLEET_ENV = os.environ.get("DOOR_FLEET_ENV", "/etc/amux/fleet.env")
AMUX = os.environ.get("DOOR_AMUX_URL", "https://127.0.0.1:8824")
MEMBER_RECHECK = float(os.environ.get("DOOR_MEMBER_RECHECK", "60"))
_member_ok_cache = {}   # token -> (ok, checked_at)


def log(**kv):
    kv["ts"] = round(time.time(), 3)
    print(json.dumps(kv), flush=True)


def scrypt_hash(password, salt=None):
    salt = salt or secrets.token_bytes(16)
    h = hashlib.scrypt(password.encode(), salt=salt, n=2**14, r=8, p=1, dklen=32)
    return "scrypt$%s$%s" % (base64.b64encode(salt).decode(), base64.b64encode(h).decode())


def users():
    out = {}
    try:
        for line in open(USERS_FILE):
            parts = line.split()
            if len(parts) == 2:
                out[parts[0]] = parts[1]
    except FileNotFoundError:
        pass
    return out


def members():
    out = {}
    try:
        for line in open(MEMBERS_FILE):
            parts = line.split()
            if len(parts) == 2:
                out[parts[0]] = parts[1]
    except FileNotFoundError:
        pass
    return out


def save_member(name, token):
    rows = {k: v for k, v in members().items() if k != name}
    rows[name] = token
    tmp = MEMBERS_FILE + ".tmp"
    with open(tmp, "w") as f:
        f.write("".join("%s %s\n" % kv for kv in rows.items()))
    os.chmod(tmp, 0o600)
    os.replace(tmp, MEMBERS_FILE)


def fleet():
    """AMUX_* settings written by cloud-init on amux Cloud servers; empty elsewhere."""
    out = {}
    try:
        for line in open(FLEET_ENV):
            k, _, v = line.strip().partition("=")
            if k:
                out[k] = json.loads(v) if v.startswith('"') else v
    except FileNotFoundError:
        pass
    return out


def _amux(method, path, data=None, cookie=None, timeout=5):
    import ssl, urllib.request
    ctx = ssl.create_default_context()
    ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE   # loopback, self-signed
    req = urllib.request.Request(AMUX + path, data=data, method=method)
    if cookie:
        req.add_header("Cookie", "amux_member=" + cookie)
    if data is not None:
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
    with urllib.request.urlopen(req, timeout=timeout, context=ctx) as r:
        return r.status, r.read()


def member_valid(token, now=None):
    """Is this amux member token still a verified member? Cached for MEMBER_RECHECK seconds."""
    now = now or time.time()
    hit = _member_ok_cache.get(token)
    if hit and now - hit[1] < MEMBER_RECHECK:
        return hit[0]
    try:
        _, body = _amux("GET", "/api/identity", cookie=token)
        ok = bool(json.loads(body).get("is_local_member"))
    except Exception:
        ok = hit[0] if hit else False      # amux briefly down: keep the last answer rather than lock everyone out
    _member_ok_cache[token] = (ok, now)
    return ok


def verify_entry_token(token, now=None):
    """The control plane's token (cloud/control/tokens.py): payload.mac, HMAC-SHA256 with this server's secret."""
    cfg = fleet()
    sec, slug = cfg.get("AMUX_FLEET_SECRET"), cfg.get("AMUX_SLUG")
    if not sec or not slug:
        raise ValueError("token sign-in is not enabled on this server")
    try:
        payload, mac = token.split(".")
    except (AttributeError, ValueError):
        raise ValueError("malformed token")
    unb64 = lambda x: base64.urlsafe_b64decode(x + "=" * (-len(x) % 4))
    if not hmac.compare_digest(hmac.new(sec.encode(), payload.encode(), hashlib.sha256).digest(), unb64(mac)):
        raise ValueError("bad signature")
    claims = json.loads(unb64(payload))
    if claims.get("exp", 0) < (now or time.time()):
        raise ValueError("expired")
    if claims.get("aud") != slug:
        raise ValueError("wrong server")
    if not claims.get("sub") or "@" not in claims["sub"]:
        raise ValueError("no person in token")
    return claims


def redeem_invite(email, invite):
    """Accept an amux invite for email; the invite token becomes the member token (as bootstrap.sh does)."""
    data = urllib.parse.urlencode({"email": email, "name": email.split("@")[0]}).encode()
    _amux("POST", "/invite/" + urllib.parse.quote(invite, safe=""), data=data, timeout=10)
    _member_ok_cache.pop(invite, None)
    if not member_valid(invite):
        raise ValueError("invite was not accepted for this email")
    save_member(email, invite)
    return invite


def check_password(user, password):
    stored = users().get(user)
    if not stored or not stored.startswith("scrypt$"):
        # Same work either way so a timing difference does not reveal users.
        scrypt_hash(password)
        return False
    _, salt, _ = stored.split("$", 2)
    return hmac.compare_digest(scrypt_hash(password, base64.b64decode(salt)), stored)


def secret():
    with open(SECRET_FILE, "rb") as f:
        return f.read().strip()


def sign(user, expiry):
    msg = ("%s.%d" % (user, expiry)).encode()
    return hmac.new(secret(), msg, hashlib.sha256).hexdigest()


def make_cookie(user, max_age=MAX_AGE):
    exp = int(time.time()) + max_age
    return "%s.%d.%s" % (user, exp, sign(user, exp))


def cookie_user(header, name=COOKIE):
    for part in (header or "").split(";"):
        k, _, v = part.strip().partition("=")
        if k == name and v:
            try:
                user, exp, mac = v.rsplit(".", 2)
                exp = int(exp)
            except ValueError:
                return None
            if exp > time.time() and hmac.compare_digest(sign(user, exp), mac) and (user in users() or user in members()):
                return user
    return None


def basic_user(header):
    if not header or not header.startswith("Basic "):
        return None
    try:
        user, _, pw = base64.b64decode(header[6:]).decode().partition(":")
    except Exception:
        return None
    return user if check_password(user, pw) else None


def session_cookies(user, max_age=MAX_AGE):
    v = make_cookie(user, max_age)
    return [("Set-Cookie", "%s=%s; Path=/; Max-Age=%d; HttpOnly; Secure; SameSite=Lax" % (COOKIE, v, max_age)),
            ("Set-Cookie", "%s=%s; Path=%s; Max-Age=%d; HttpOnly; Secure; SameSite=None" % (VIEW_COOKIE, v, VIEW_PATH, max_age)),
            ("Set-Cookie", "%s=1; Path=/; Max-Age=%d; HttpOnly; Secure; SameSite=Lax" % (VIEW_MARK, max_age))]


def has_cookie(header, name):
    return any(p.strip().partition("=")[0] == name for p in (header or "").split(";"))


def safe_next(n):
    # Only a same-host path: never an open redirect.
    n = n or "/"
    return n if n.startswith("/") and not n.startswith("//") else "/"


PAGE = """<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>Sign in to amux</title>
<style>
body{margin:0;font:16px -apple-system,system-ui,sans-serif;background:#0d1117;color:#e6edf3;display:flex;min-height:100vh;align-items:center;justify-content:center}
form{background:#161b22;border:1px solid #30363d;border-radius:12px;padding:28px;width:min(340px,90vw)}
h1{font-size:20px;margin:0 0 4px}p{color:#8b949e;font-size:13px;margin:0 0 18px}
label{display:block;font-size:13px;color:#8b949e;margin:12px 0 4px}
input{width:100%%;box-sizing:border-box;min-height:44px;padding:10px;border-radius:8px;border:1px solid #30363d;background:#0d1117;color:#e6edf3;font-size:16px}
button{margin-top:18px;width:100%%;min-height:44px;border:0;border-radius:8px;background:#58a6ff;color:#0d1117;font-weight:600;font-size:16px}
.err{color:#f85149;font-size:13px;margin-top:10px}
</style></head><body>
<form method="post" action="/_door/login">
<h1>amux</h1><p>%(host)s</p>
<label for="u">Username</label><input id="u" name="username" autocomplete="username" autocapitalize="none" required value="%(user)s">
<label for="p">Password</label><input id="p" name="password" type="password" autocomplete="current-password" required>
<input type="hidden" name="next" value="%(next)s">
<button type="submit">Sign in</button>%(err)s
</form></body></html>"""


class Door(BaseHTTPRequestHandler):
    server_version = "amux-door"

    def log_message(self, *a):
        pass

    def send(self, code, body=b"", headers=()):
        self.send_response(code)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def page(self, nxt, user="", err=""):
        host = html.escape(self.headers.get("X-Forwarded-Host") or self.headers.get("Host") or "")
        body = PAGE % {"host": host, "next": html.escape(safe_next(nxt)), "user": html.escape(user),
                       "err": '<div class="err">%s</div>' % html.escape(err) if err else ""}
        self.send(401 if err else 200, body.encode(), [("Content-Type", "text/html; charset=utf-8"),
                                                       ("Cache-Control", "no-store")])

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path)
        if path.path == "/_door/check":
            # forward_auth: the original request's method/URI arrive as headers.
            uri = self.headers.get("X-Forwarded-Uri", "/")
            method = self.headers.get("X-Forwarded-Method", "GET")
            cookies = self.headers.get("Cookie")
            user = cookie_user(cookies) or basic_user(self.headers.get("Authorization"))
            if not user and method == "GET" and uri.startswith(VIEW_PATH):
                user = cookie_user(cookies, VIEW_COOKIE)
            if user:
                tok = members().get(user)
                if user not in users() and not (tok and member_valid(tok)):
                    log(event="check", user=user, verdict="door_member_revoked")
                    user = None
            if user:
                # Signed in before the view cookie existed: one hop to add it.
                if (method == "GET" and "text/html" in self.headers.get("Accept", "")
                        and not has_cookie(cookies, VIEW_MARK) and not uri.startswith(("/_door/", VIEW_PATH))):
                    loc = "/_door/refresh?next=" + urllib.parse.quote(safe_next(uri), safe="")
                    return self.send(302, headers=[("Location", loc), ("Cache-Control", "no-store")])
                tok = members().get(user)
                return self.send(200, headers=[("X-Door-User", user)] + ([("X-Amux-Member", tok)] if tok else []))
            accept = self.headers.get("Accept", "")
            if self.headers.get("X-Forwarded-Method", "GET") == "GET" and "text/html" in accept:
                cfg = fleet()
                if cfg.get("AMUX_CONTROL_URL") and cfg.get("AMUX_SLUG") and not users():
                    # amux Cloud: people arrive through cloud.amux.io, which sends them back with a fresh token.
                    loc = "%s/?server=%s" % (cfg["AMUX_CONTROL_URL"].rstrip("/"), urllib.parse.quote(cfg["AMUX_SLUG"]))
                    return self.send(302, headers=[("Location", loc), ("Cache-Control", "no-store")])
                loc = "/_door/login?next=" + urllib.parse.quote(safe_next(uri), safe="")
                return self.send(302, headers=[("Location", loc)])
            return self.send(401, b'{"error":"sign in required"}', [("Content-Type", "application/json"),
                             ("WWW-Authenticate", 'Basic realm="amux"')])
        if path.path == "/_door/enter":
            q = urllib.parse.parse_qs(path.query)
            ip = self.headers.get("X-Forwarded-For", self.client_address[0])
            try:
                claims = verify_entry_token(q.get("t", [""])[0])
                email = claims["sub"].lower()
                known = members().get(email)
                if claims.get("invite") and not (known and member_valid(known)):
                    redeem_invite(email, claims["invite"])     # invites are single use; a member skips this
                tok = members().get(email)
                if not tok or not member_valid(tok):
                    raise ValueError("not a member of this server")
            except Exception as e:
                log(event="enter", ip=ip, verdict="door_enter_refused", reason=str(e)[:120])
                body = ("<!doctype html><meta charset=utf-8><title>Link expired</title><p>This sign-in link has expired "
                        "or is not for this server. <a href='%s'>Sign in again</a></p>" %
                        html.escape(fleet().get("AMUX_CONTROL_URL", "https://cloud.amux.io"))).encode()
                return self.send(401, body, [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])
            log(event="enter", user=email, ip=ip, role=claims.get("role"), verdict="door_enter_ok")
            nxt = safe_next(q.get("next", ["/"])[0])
            return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] +
                             session_cookies(email, TOKEN_MAX_AGE))
        if path.path == "/_door/login":
            q = urllib.parse.parse_qs(path.query)
            return self.page(q.get("next", ["/"])[0])
        if path.path == "/_door/refresh":
            q = urllib.parse.parse_qs(path.query)
            nxt = safe_next(q.get("next", ["/"])[0])
            user = cookie_user(self.headers.get("Cookie"))
            if not user:
                return self.send(302, headers=[("Location", "/_door/login?next=" + urllib.parse.quote(nxt, safe=""))])
            return self.send(302, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(user))
        if path.path == "/_door/logout":
            return self.send(302, headers=[("Location", "/_door/login"),
                             ("Set-Cookie", "%s=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax" % COOKIE),
                             ("Set-Cookie", "%s=; Path=%s; Max-Age=0; HttpOnly; Secure; SameSite=None" % (VIEW_COOKIE, VIEW_PATH)),
                             ("Set-Cookie", "%s=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax" % VIEW_MARK)])
        self.send(404)

    def do_POST(self):
        if urllib.parse.urlsplit(self.path).path != "/_door/login":
            return self.send(404)
        n = int(self.headers.get("Content-Length") or 0)
        form = urllib.parse.parse_qs(self.rfile.read(min(n, 65536)).decode(errors="replace"))
        user = (form.get("username") or [""])[0].strip()
        pw = (form.get("password") or [""])[0]
        nxt = safe_next((form.get("next") or ["/"])[0])
        ip = self.headers.get("X-Forwarded-For", self.client_address[0])
        if check_password(user, pw):
            log(event="signin", user=user, ip=ip, verdict="door_signin_ok")
            return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(user))
        log(event="signin", user=user, ip=ip, verdict="door_signin_refused")
        time.sleep(1.0)  # slows guessing without a lockout a person could trip
        return self.page(nxt, user, "Wrong username or password.")


def main():
    if len(sys.argv) >= 3 and sys.argv[1] == "hash":
        print("%s %s" % (sys.argv[2], scrypt_hash(sys.stdin.readline().rstrip("\n"))))
        return
    if len(sys.argv) >= 2 and sys.argv[1] == "serve":
        log(event="start", port=PORT, users=len(users()), verdict="door_started")
        ThreadingHTTPServer(("127.0.0.1", PORT), Door).serve_forever()
        return
    sys.exit(__doc__)


if __name__ == "__main__":
    main()
