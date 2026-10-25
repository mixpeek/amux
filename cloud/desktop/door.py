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

The owner (an AMUX_WEB_USERS entry marked `owner`, which is the paying customer on amux
Cloud) has no member token: Caddy then sends amux an empty member cookie and amux treats
the request as its host owner, who can invite teammates in Settings. A teammate opens the
invite link (/invite/<token>), the door sends them to /_door/join, where they pick their
own password; the door redeems the invite and keeps that password in JOINED_FILE.

Every other account, password or token, maps to an amux member token in MEMBERS_FILE
("name token" lines). /_door/check returns it as X-Amux-Member, which Caddy
forwards as the only cookie amux sees. Each token is re-validated against
amux-server's /api/identity at most every 60 seconds, so revoking a member in
Team settings locks them out within a minute.

Activation report (amux Cloud): GET /_door/report with `Authorization: Bearer <token>`, a
two-minute token signed like a sign-in link but with scope "report" and no person. It returns
counts only (sign-ins, connected model providers as booleans, workers, cards, schedules, error
groups, disk), never content, so cloud.amux.io can tell a customer who is stuck in setup from one
who is working. Sign-in counts live in ACTIVITY_FILE, written by this process.

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
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def tls_days_left(host):
    """Days until the certificate Caddy serves for this host expires, verified the way a browser
    would, or None when it cannot be checked. Caddy renews about 30 days out; the Cloud monitor
    emails the operator when this falls under 14, weeks before customers would see an expired
    certificate (C9)."""
    import socket
    import ssl
    host = host.split(":")[0].strip().lower()
    if not host or len(host) > 253 or not all(c.isalnum() or c in ".-" for c in host):
        return None
    try:
        ctx = ssl.create_default_context()
        with socket.create_connection(("127.0.0.1", 443), timeout=3) as raw:
            with ctx.wrap_socket(raw, server_hostname=host) as tls:
                not_after = tls.getpeercert()["notAfter"]
        return int((ssl.cert_time_to_seconds(not_after) - time.time()) // 86400)
    except ssl.SSLCertVerificationError:
        return -1   # expired or otherwise invalid: as bad as it gets
    except Exception:
        return None

USERS_FILE = os.environ.get("DOOR_USERS", "/etc/amux-door/users")
SECRET_FILE = os.environ.get("DOOR_SECRET", "/etc/amux-door/secret")
UPDATE_STATUS = os.environ.get("DOOR_UPDATE_STATUS", "/var/lib/amux-update/status.json")
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
JOINED_FILE = os.environ.get("DOOR_JOINED", "/etc/amux-door/joined")   # teammates' own passwords
FLEET_ENV = os.environ.get("DOOR_FLEET_ENV", "/etc/amux/fleet.env")
ENTRY_FILE = os.environ.get("DOOR_ENTRY", "/etc/amux-door/entry")    # standalone: this server's app sign-in key
AMUX = os.environ.get("DOOR_AMUX_URL", "https://127.0.0.1:8824")
MEMBER_RECHECK = float(os.environ.get("DOOR_MEMBER_RECHECK", "60"))
# Sign-in counts for the activation report. /etc/amux-door is group-writable by amux-door on every
# server already, so a self-updated door can keep it without a bootstrap change.
ACTIVITY_FILE = os.environ.get("DOOR_ACTIVITY", "/etc/amux-door/activity.json")
SEEN_FLUSH_S = 600   # a signed-in visit is noted in memory on every request, written at most every 10 min
_member_ok_cache = {}   # token -> (ok, checked_at)


def log(**kv):
    kv["ts"] = round(time.time(), 3)
    print(json.dumps(kv), flush=True)


def scrypt_hash(password, salt=None):
    salt = salt or secrets.token_bytes(16)
    h = hashlib.scrypt(password.encode(), salt=salt, n=2**14, r=8, p=1, dklen=32)
    return "scrypt$%s$%s" % (base64.b64encode(salt).decode(), base64.b64encode(h).decode())


def users(path=None):
    out = {}
    try:
        for line in open(path or USERS_FILE):
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


def joined():
    """Teammates who set their password at /_door/join. They stay members: revoking one in
    Settings locks them out, because their check goes through member_valid like a token user."""
    return users(JOINED_FILE)


def save_joined(name, pw_hash):
    rows = {k: v for k, v in joined().items() if k != name}
    rows[name] = pw_hash
    tmp = JOINED_FILE + ".tmp"
    with open(tmp, "w") as f:
        f.write("".join("%s %s\n" % kv for kv in rows.items()))
    os.chmod(tmp, 0o600)
    os.replace(tmp, JOINED_FILE)


def save_member(name, token):
    rows = {k: v for k, v in members().items() if k != name}
    rows[name] = token
    tmp = MEMBERS_FILE + ".tmp"
    with open(tmp, "w") as f:
        f.write("".join("%s %s\n" % kv for kv in rows.items()))
    os.chmod(tmp, 0o600)
    os.replace(tmp, MEMBERS_FILE)


def fleet(path=None):
    """AMUX_* settings written by cloud-init on amux Cloud servers; empty elsewhere."""
    out = {}
    try:
        for line in open(path or FLEET_ENV):
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


def _signed_claims(token, now=None):
    """payload.mac, HMAC-SHA256 with this server's key: checks the signature, expiry and server."""
    cfg = fleet() if fleet().get("AMUX_FLEET_SECRET") else fleet(ENTRY_FILE)
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
    return claims


def verify_entry_token(token, now=None):
    """A sign-in link's token: payload.mac, HMAC-SHA256 with this server's secret. Standalone servers get
    the secret from cloud.amux.io at creation (ENTRY_FILE; the iOS app's Sign in with Apple), fleet
    servers from fleet.env."""
    claims = _signed_claims(token, now)
    if claims.get("scope"):
        raise ValueError("not a sign-in token")   # a report token must never sign anyone in
    if not claims.get("sub") or "@" not in claims["sub"]:
        raise ValueError("no person in token")
    return claims


def verify_report_token(token, now=None):
    """The activation report's token: same key and format, scope "report", at most 5 minutes long."""
    claims = _signed_claims(token, now)
    if claims.get("scope") != "report":
        raise ValueError("not a report token")
    if claims.get("exp", 0) > (now or time.time()) + 300:
        raise ValueError("report token lives too long")
    return claims


# -- activity: sign-in counts for the activation report (no names, no addresses) --
_activity_lock = threading.Lock()
_activity = None
_activity_flushed = 0.0


def _activity_load(now):
    global _activity
    if _activity is None:
        try:
            _activity = json.load(open(ACTIVITY_FILE))
        except Exception:
            _activity = {}
        _activity.setdefault("since", int(now))   # when counting began: earlier sign-ins are unknown
        for k in ("ok", "first_ok", "last_ok", "last_seen"):
            _activity.setdefault(k, 0)
        _activity.setdefault("failed", [])
    return _activity


def _activity_flush(now):
    global _activity_flushed
    try:
        tmp = ACTIVITY_FILE + ".tmp"
        with open(tmp, "w") as f:
            json.dump(_activity, f)
        os.replace(tmp, ACTIVITY_FILE)
        _activity_flushed = now
    except Exception as e:
        log(event="activity", verdict="door_activity_write_failed", error=str(e)[:120])


def note_signin(ok, now=None):
    """A sign-in by password, link or invite. Counted; who it was is not kept."""
    now = now or time.time()
    with _activity_lock:
        a = _activity_load(now)
        if ok:
            a["ok"] += 1
            a["first_ok"] = a["first_ok"] or int(now)
            a["last_ok"] = a["last_seen"] = int(now)
        else:
            a["failed"] = [t for t in a["failed"] if t > now - 86400][-999:] + [int(now)]
        _activity_flush(now)


def note_seen(now=None):
    """A request from someone signed in (a 30-day cookie outlives its sign-in, and that is still use)."""
    now = now or time.time()
    with _activity_lock:
        a = _activity_load(now)
        a["last_seen"] = int(now)
        if now - _activity_flushed > SEEN_FLUSH_S:
            _activity_flush(now)


def _amux_json(path, timeout=10):
    code, body = _amux("GET", path, timeout=timeout)
    if code != 200:
        raise ValueError("HTTP %d" % code)
    return json.loads(body)


REPORT_SECTIONS = ("signin", "model", "workers", "tokens", "board", "schedules", "errors", "version", "disk")


def activation_report(now=None, get=None, disk_path="/"):
    """Counts and booleans only. Never message text, prompts, names, emails, titles, paths or keys:
    every value below is a number, a boolean, a provider name or the running commit."""
    now = now or time.time()
    get = get or _amux_json

    def signin():
        with _activity_lock:
            a = dict(_activity_load(now))
        return {"tracking_since": a["since"], "ever": bool(a["ok"] or a["last_seen"]), "count": a["ok"],
                "first_at": a["first_ok"] or None, "last_at": a["last_ok"] or None,
                "last_seen_at": a["last_seen"] or None,
                "failed_24h": sum(1 for t in a["failed"] if t > now - 86400)}

    def model():
        auth = get("/api/identity").get("model_auth") or {}
        providers = {p: any(v is True for v in f.values()) for p, f in auth.items() if isinstance(f, dict)}
        return {"any": any(providers.values()), "providers": providers}

    def workers():
        rows = [x for x in get("/api/sessions") if isinstance(x, dict)]
        live = [x for x in rows if not x.get("archived")]
        used = lambda x: ((x.get("tokens") or {}).get("total") or 0) > 0
        recent = lambda x: (x.get("last_activity") or 0) > now - 86400
        return {"n_considered": len(rows), "total": len(live), "ever_ran": sum(1 for x in rows if used(x)),
                "active_24h": sum(1 for x in live if used(x) and recent(x)),
                "running": sum(1 for x in live if x.get("running"))}

    def tokens():
        return {"today": int(get("/api/stats/daily").get("total_tokens") or 0)}

    def board():
        rows = [r for r in get("/api/board?all=1") if isinstance(r, dict)]
        return {"n_considered": len(rows), "cards": sum(1 for r in rows if not r.get("archived"))}

    def schedules():
        rows = [r for r in get("/api/schedules") if isinstance(r, dict)]
        return {"total": len(rows), "enabled": sum(1 for r in rows if r.get("enabled"))}

    def errors():
        a = get("/api/logs/analyze?since_h=24", timeout=20)
        if not a.get("measured"):
            raise ValueError("log analysis did not run: %s" % str(a.get("why_unmeasured", ""))[:80])
        return {"groups_24h": int(a.get("groups_total") or len(a.get("groups") or [])),
                "total_24h": int(a.get("total_errors") or 0), "n_considered": int(a.get("n_considered") or 0)}

    def version():
        return {"commit": str(get("/health", timeout=3).get("commit", ""))[:40]}

    def disk():
        st = os.statvfs(disk_path)
        total = st.f_blocks * st.f_frsize
        if not total:
            raise ValueError("no blocks")
        return {"used_pct": round(100.0 * (total - st.f_bavail * st.f_frsize) / total, 1)}

    fns = {"signin": signin, "model": model, "workers": workers, "tokens": tokens, "board": board,
           "schedules": schedules, "errors": errors, "version": version, "disk": disk}
    out, missing = {"v": 1, "generated_at": int(now)}, []
    for name in REPORT_SECTIONS:
        try:
            out[name] = dict(fns[name](), measured=True)
        except Exception as e:
            out[name] = {"measured": False, "why_unmeasured": str(e)[:120]}
            missing.append(name)
    # Diagnostic contract: a zero is "measured and zero" only beside measured: true.
    out["n_considered"] = len(REPORT_SECTIONS)
    out["measured"] = not missing
    out["unmeasured"] = missing
    return out


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
    stored = users().get(user) or joined().get(user)
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

JOIN_PAGE = PAGE.replace('<form method="post" action="/_door/login">', '<form method="post" action="/_door/join">').replace(
    "<title>Sign in to amux</title>", "<title>Join amux</title>").replace(
    '<h1>amux</h1><p>%(host)s</p>', '<h1>Join %(host)s</h1><p>You were invited to this amux server. Choose the password you will sign in with.</p>').replace(
    '<label for="u">Username</label>', '<label for="u">Your email</label>').replace(
    'autocomplete="current-password" required>', 'autocomplete="new-password" minlength="10" required>'
    '<label for="p2">Password again</label><input id="p2" name="password2" type="password" autocomplete="new-password" required>').replace(
    '<input type="hidden" name="next" value="%(next)s">', '<input type="hidden" name="invite" value="%(next)s">').replace(
    '<button type="submit">Sign in</button>', '<button type="submit">Join</button>')


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

    def join_page(self, invite, user="", err=""):
        host = html.escape(self.headers.get("X-Forwarded-Host") or self.headers.get("Host") or "")
        body = JOIN_PAGE % {"host": host, "next": html.escape(invite), "user": html.escape(user),
                            "err": '<div class="err">%s</div>' % html.escape(err) if err else ""}
        self.send(400 if err else 200, body.encode(), [("Content-Type", "text/html; charset=utf-8"),
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
                note_seen()
                tok = members().get(user)
                return self.send(200, headers=[("X-Door-User", user)] + ([("X-Amux-Member", tok)] if tok else []))
            accept = self.headers.get("Accept", "")
            if method == "GET" and uri.startswith("/invite/"):
                # A teammate's invite link (Settings > Team). They have no password yet.
                inv = uri[len("/invite/"):].split("?")[0].split("/")[0]
                return self.send(302, headers=[("Location", "/_door/join?invite=" + urllib.parse.quote(inv, safe="")),
                                               ("Cache-Control", "no-store")])
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
                if email in users():       # the server's owner (a standalone server's paying customer)
                    log(event="enter", user=email, ip=ip, role="owner", via=claims.get("via"), verdict="door_enter_ok")
                    note_signin(True)
                    nxt = safe_next(q.get("next", ["/"])[0])
                    return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(email))
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
                        html.escape(fleet().get("AMUX_CONTROL_URL", "/_door/login"))).encode()
                return self.send(401, body, [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])
            log(event="enter", user=email, ip=ip, role=claims.get("role"), verdict="door_enter_ok")
            note_signin(True)
            nxt = safe_next(q.get("next", ["/"])[0])
            return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] +
                             session_cookies(email, TOKEN_MAX_AGE))
        if path.path == "/_door/login":
            q = urllib.parse.parse_qs(path.query)
            return self.page(q.get("next", ["/"])[0])
        if path.path == "/_door/join":
            q = urllib.parse.parse_qs(path.query)
            return self.join_page(q.get("invite", [""])[0])
        if path.path == "/_door/status":
            # Public and secret-free: the self-updater's last outcome (cloud/desktop/update.sh) plus
            # whether amux answers right now. cloud.amux.io's monitor reads it without signing in.
            try:
                st = json.load(open(UPDATE_STATUS))
            except Exception:
                st = {"verdict": "unknown"}
            try:
                st["live_commit"] = json.loads(_amux("GET", "/health", timeout=3)[1]).get("commit", "")
            except Exception:
                st["live_commit"] = ""
            try:   # the page a customer lands on, fetched as the owner over loopback
                code, body = _amux("GET", "/", timeout=5)
                st["dashboard"] = code == 200 and b"<html" in body[:4096].lower()
            except Exception:
                st["dashboard"] = False
            st["tls_days_left"] = tls_days_left(self.headers.get("Host", ""))
            st["measured"] = True
            return self.send(200, json.dumps(st).encode(), [("Content-Type", "application/json"),
                                                             ("Cache-Control", "no-store")])
        if path.path == "/_door/report":
            # Counts only, for cloud.amux.io's monitor. Anything without a valid report token gets the
            # same 404 as an unknown path, so the endpoint reveals nothing.
            auth = self.headers.get("Authorization", "")
            try:
                verify_report_token(auth[7:] if auth.startswith("Bearer ") else "")
            except Exception as e:
                log(event="report", verdict="door_report_refused", reason=str(e)[:120])
                return self.send(404)
            rep = activation_report()
            log(event="report", verdict="door_report_ok", measured=rep["measured"], unmeasured=rep["unmeasured"])
            return self.send(200, json.dumps(rep).encode(), [("Content-Type", "application/json"),
                                                              ("Cache-Control", "no-store")])
        if path.path == "/_door/refresh":
            q = urllib.parse.parse_qs(path.query)
            nxt = safe_next(q.get("next", ["/"])[0])
            user = cookie_user(self.headers.get("Cookie"))
            if not user:
                return self.send(302, headers=[("Location", "/_door/login?next=" + urllib.parse.quote(nxt, safe=""))])
            return self.send(302, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(user))
        if path.path == "/_door/logout":
            # To "/", so the check decides: an amux Cloud server goes to cloud.amux.io sign-in,
            # a password host to its login form (the dashboard's Sign out, 2026-10-17).
            return self.send(302, headers=[("Location", "/"), ("Cache-Control", "no-store"),
                             ("Set-Cookie", "%s=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax" % COOKIE),
                             ("Set-Cookie", "%s=; Path=%s; Max-Age=0; HttpOnly; Secure; SameSite=None" % (VIEW_COOKIE, VIEW_PATH)),
                             ("Set-Cookie", "%s=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax" % VIEW_MARK)])
        self.send(404)

    def do_POST(self):
        route = urllib.parse.urlsplit(self.path).path
        if route not in ("/_door/login", "/_door/join"):
            return self.send(404)
        n = int(self.headers.get("Content-Length") or 0)
        form = urllib.parse.parse_qs(self.rfile.read(min(n, 65536)).decode(errors="replace"))
        if route == "/_door/join":
            return self.join(form)
        user = (form.get("username") or [""])[0].strip()
        pw = (form.get("password") or [""])[0]
        nxt = safe_next((form.get("next") or ["/"])[0])
        ip = self.headers.get("X-Forwarded-For", self.client_address[0])
        if check_password(user, pw):
            log(event="signin", user=user, ip=ip, verdict="door_signin_ok")
            note_signin(True)
            return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(user))
        log(event="signin", user=user, ip=ip, verdict="door_signin_refused")
        note_signin(False)
        time.sleep(1.0)  # slows guessing without a lockout a person could trip
        return self.page(nxt, user, "Wrong username or password.")


    def join(self, form):
        email = (form.get("username") or [""])[0].strip().lower()
        pw, pw2 = (form.get("password") or [""])[0], (form.get("password2") or [""])[0]
        invite = (form.get("invite") or [""])[0].strip()
        ip = self.headers.get("X-Forwarded-For", self.client_address[0])
        if "@" not in email or not (3 <= len(email) <= 200) or any(c.isspace() for c in email):
            return self.join_page(invite, email, "Enter your email address.")
        if len(pw) < 10:
            return self.join_page(invite, email, "Use at least 10 characters.")
        if pw != pw2:
            return self.join_page(invite, email, "The two passwords differ.")
        if email in users() or email in joined():
            return self.join_page(invite, email, "That email already has an account here. Sign in instead.")
        try:
            redeem_invite(email, invite)
        except Exception as e:
            log(event="join", user=email, ip=ip, verdict="door_join_refused", reason=str(e)[:120])
            time.sleep(1.0)
            return self.join_page(invite, email, "This invite link is used or no longer valid. Ask the server owner for a new one.")
        save_joined(email, scrypt_hash(pw))
        log(event="join", user=email, ip=ip, verdict="door_join_ok")
        note_signin(True)
        return self.send(303, headers=[("Location", "/"), ("Cache-Control", "no-store")] + session_cookies(email))


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
