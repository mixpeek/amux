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
# Wrong passwords from one address, form or Basic, before that address is refused for the rest
# of the window. Before this, Basic credentials on any URL could be guessed as fast as scrypt
# allowed (2026-10-31). Per address, so a stranger guessing never locks the owner out.
FAIL_LIMIT = int(os.environ.get("DOOR_FAIL_LIMIT", "10"))
FAIL_WINDOW_S = int(os.environ.get("DOOR_FAIL_WINDOW_S", "900"))
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


def _amux(method, path, data=None, cookie=None, timeout=5, content_type="application/x-www-form-urlencoded"):
    import ssl, urllib.request
    ctx = ssl.create_default_context()
    ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE   # loopback, self-signed
    req = urllib.request.Request(AMUX + path, data=data, method=method)
    if cookie:
        req.add_header("Cookie", "amux_member=" + cookie)
    if data is not None:
        req.add_header("Content-Type", content_type)
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


_fails, _fails_lock = {}, threading.Lock()


def client_ip(headers, peer):
    """The visitor's address. Caddy overwrites X-Real-IP with {client_ip}, which behind
    Cloudflare is CF-Connecting-IP from a Cloudflare address only; else the first hop."""
    real = (headers.get("X-Real-IP") or "").strip()
    if real:
        return real
    return (headers.get("X-Forwarded-For") or "").split(",")[0].strip() or peer


def throttled(ip, now=None):
    """Seconds until `ip` may try a password again, or 0."""
    now = now or time.time()
    with _fails_lock:
        recent = [t for t in _fails.get(ip, []) if now - t < FAIL_WINDOW_S]
        _fails[ip] = recent
        if len(recent) < FAIL_LIMIT:
            return 0
        return int(recent[0] + FAIL_WINDOW_S - now) + 1


def note_failure(ip, now=None):
    now = now or time.time()
    with _fails_lock:
        if len(_fails) > 10000:   # bounded: forget the quietest addresses first
            for k in sorted(_fails, key=lambda k: _fails[k][-1] if _fails[k] else 0)[:5000]:
                _fails.pop(k, None)
        _fails.setdefault(ip, []).append(now)
        n = len(_fails[ip])
    if n == FAIL_LIMIT:
        log(event="throttle", ip=ip, failures=n, window_s=FAIL_WINDOW_S, verdict="door_signin_throttled")


# ── Address allowlist (Ethan, 2026-10-31: "whitelist only the ips that have previously connected so
# that any other ips get blocked"; "for the first signup/signin users can whitelist their ip").
# Off until someone turns it on: the owner at their first sign-in (a checkbox), a signup that asked
# for it (AMUX_IP_ALLOWLIST, written by bootstrap.sh), the owner's page at /_door/allowlist, or
# `door.py allowlist ...` over SSH. When on, /_door/check refuses every address not on the list,
# signed in or not. A person whose account has no address yet adds theirs by signing in (the first
# sign-in), unless the owner turned that off. Public /_door/* pages, public links (/pub/<id>/) and
# /_door/status stay reachable: the owner published those, and cloud.amux.io's monitor reads status.
ALLOW_FILE = os.environ.get("DOOR_ALLOWLIST", "/etc/amux-door/allowlist.json")
_allow_cache = {"mtime": None, "data": {}}
_allow_lock = threading.RLock()
_blocked_logged = {}


def allowlist():
    """{"enabled", "first_signin", "admins", "entries": [{ip, user, how, added, note}]}. A file that
    cannot be parsed counts as ON with no entries (closed), and says so in the log."""
    try:
        m = os.stat(ALLOW_FILE).st_mtime_ns
    except FileNotFoundError:
        return {}
    if _allow_cache["mtime"] != m:
        try:
            with open(ALLOW_FILE) as f:
                data = json.load(f)
            if not isinstance(data, dict):
                raise ValueError("not an object")
        except (OSError, ValueError) as e:
            log(event="allowlist", verdict="door_allowlist_unreadable", reason=str(e)[:120])
            data = {"enabled": True, "entries": [], "unreadable": True}
        _allow_cache.update(mtime=m, data=data)
    return _allow_cache["data"]


def save_allowlist(data):
    tmp = ALLOW_FILE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=1)
    os.chmod(tmp, 0o640)
    if os.geteuid() == 0:   # written by root over SSH: the door must still read and rewrite it
        import pwd
        try:
            pw = pwd.getpwnam("amux-door")
            os.chown(tmp, pw.pw_uid, pw.pw_gid)
        except KeyError:
            pass
    os.replace(tmp, ALLOW_FILE)
    _allow_cache["mtime"] = None


def _net(text):
    import ipaddress
    return ipaddress.ip_network(text.strip(), strict=False)


def ip_listed(ip, data=None):
    import ipaddress
    data = allowlist() if data is None else data
    try:
        a = ipaddress.ip_address(ip)
    except ValueError:
        return False
    if a.is_loopback:
        return True
    for e in data.get("entries", []):
        try:
            if a in _net(e.get("ip", "")):
                return True
        except ValueError:
            continue
    return False


def ip_allowed(ip, data=None):
    data = allowlist() if data is None else data
    return not data.get("enabled") or ip_listed(ip, data)


def admins(data=None):
    """Who may edit the list: the names in the file, else the host's owners (password users with no
    member token, which on amux Cloud is the paying customer)."""
    data = allowlist() if data is None else data
    if data.get("admins"):
        return set(data["admins"])
    toks = members()
    return {u for u in users() if u not in toks}


def update_allowlist(change):
    """Read the list fresh, apply change(data) and save, all under one lock, so two sign-ins at the
    same moment both keep their address (a snapshot read outside the lock dropped one)."""
    with _allow_lock:
        _allow_cache["mtime"] = None
        data = json.loads(json.dumps(allowlist()))
        data.pop("unreadable", None)
        data.setdefault("entries", [])
        out = change(data)
        if out is not False:
            save_allowlist(data)
        return out


def allow_add(ip, user, how, note=""):
    """Add `ip` (an address or a CIDR) for `user`. Returns the stored entry, or None if it was listed."""
    net = str(_net(ip))

    def add(data):
        if any(str(_net(e["ip"])) == net for e in data["entries"] if e.get("ip")):
            return False
        entry = {"ip": net if "/" in ip else ip.strip(), "user": user, "how": how,
                 "added": int(time.time()), "note": note[:120]}
        data["entries"].append(entry)
        return entry
    entry = update_allowlist(add)
    if not entry:
        return None
    log(event="allowlist", ip=entry["ip"], user=user, how=how, verdict="door_allowlist_added")
    return entry


def first_signin_add(user, ip):
    """On: an address not on the list may sign in only as an account with no address yet (its first
    sign-in), which adds the address. True when the address may continue."""
    data = allowlist()
    if not data.get("enabled") or ip_listed(ip, data):
        return True
    if data.get("first_signin", True) and not data.get("unreadable") \
            and not any(e.get("user") == user for e in data.get("entries", [])):
        allow_add(ip, user, "first_signin")
        log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_first_signin")
        return True
    log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_signin_refused")
    request_access(user, ip)
    alert_refused_signin(user, ip)
    return False


# A person with a valid credential on a new address used to reach the owner as a complaint that the
# password did not work, and the owner had to find the address by hand (velocity audit 2026-11-02,
# B4). Only code past the credential check gets here, so a guess cannot page anyone. The page the
# person sees stays the same for a right and a wrong password, and the send runs on its own thread
# so a right password takes no longer. The alert names the address and has no allow link: a link
# would be built from a Host header, and a stolen password should not be one tap from access.
REFUSED_ALERTS_PER_DAY = int(os.environ.get("DOOR_REFUSED_ALERTS_PER_DAY", "5"))
_refused_alerts = {"ips": {}, "sent": []}
_refused_alerts_lock = threading.Lock()


def alert_refused_signin(user, ip, now=None):
    """Tell the owner once per address per hour, at most REFUSED_ALERTS_PER_DAY a day. True if sent."""
    now = now or time.time()
    with _refused_alerts_lock:
        if now - _refused_alerts["ips"].get(ip, 0) < 3600:
            return False
        sent = [t for t in _refused_alerts["sent"] if now - t < 86400]
        if len(sent) >= REFUSED_ALERTS_PER_DAY:
            _refused_alerts["sent"] = sent
            log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_alert_capped")
            return False
        _refused_alerts["sent"] = sent + [now]
        _refused_alerts["ips"] = {k: v for k, v in _refused_alerts["ips"].items() if now - v < 3600}
        _refused_alerts["ips"][ip] = now
    threading.Thread(target=_send_refused_alert, args=(user, ip), daemon=True).start()
    return True


def _send_refused_alert(user, ip):
    body = {"session": "amux-door", "reason": "a valid sign-in was refused by the address allowlist",
            "message": "%s signed in correctly from %s, which is not on this server's allowlist, so they were "
                       "refused. It is waiting in Needs you: if that is them, tap Allow at /_door/allowlist, or over SSH: "
                       "sudo python3 /opt/amux-door/door.py allowlist add %s. If it is not them, their password "
                       "is known to someone else." % (user, ip, ip)}
    try:
        _amux("POST", "/api/alert/owner", data=json.dumps(body).encode(), timeout=10, content_type="application/json")
        log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_alert_sent")
    except Exception as e:
        log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_alert_failed", reason=str(e)[:120])


# ── A refused sign-in is a request the owner answers (Ethan, 2026-11-05, after Doron was locked out
# by a new address and the alert above reached no channel: "add alerts here so i receive it. should
# be a needs you"; "it just needs to be more flexible ... but still secure"). Only code past the
# credential check gets here, so a guess files nothing. The request is kept in the allowlist file
# and filed once per (account, address) as a needs-you card on this server's board, where the owner
# already looks. Allowing it still takes the owner, signed in from a listed address, pressing Allow
# on /_door/allowlist (CSRF-checked POST); the card's link is relative, so no Host header builds it.
MAX_REQUESTS = 20


def request_access(user, ip, now=None):
    """Record (user, ip) as waiting for the owner, and file its card the first time. Returns the request."""
    now = int(now or time.time())

    def add(data):
        reqs = data.setdefault("requests", [])
        for r in reqs:
            if r.get("ip") == ip and r.get("user") == user:
                r["last"], r["count"] = now, r.get("count", 1) + 1
                return dict(r, new=False)
        r = {"ip": ip, "user": user, "first": now, "last": now, "count": 1}
        reqs.append(r)
        del reqs[:-MAX_REQUESTS]    # the oldest falls off; a sign-in files it again
        return dict(r, new=True)
    req = update_allowlist(add)
    log(event="allowlist", user=user, ip=ip, verdict="door_access_requested" if req["new"] else "door_access_rerequested")
    if req["new"]:
        threading.Thread(target=_file_access_card, args=(user, ip), daemon=True).start()
    return req


def access_card_body(user, ip):
    return {"title": "Let %s sign in from %s?" % (user, ip), "status": "needsyou", "tags": ["door-access"],
            "ask_type": "access", "ask_actor": "server owner",
            "ask_question": "Let %s sign in to this server from the new address %s?" % (user, ip),
            "ask_unblocks": "Allowing it adds %s to the address allowlist, and %s can sign in from it." % (ip, user),
            "desc": "%s signed in with the right password from %s, which is not on this server's address "
                    "allowlist, so the door refused them.\n\nIf that is them, open [the address allowlist]"
                    "(/_door/allowlist) and press Allow beside %s. Over SSH: `sudo python3 /opt/amux-door/door.py "
                    "allowlist add %s`.\n\nIf it is not them, press Dismiss there and change their password: "
                    "someone else knows it." % (user, ip, ip, ip)}


def _file_access_card(user, ip):
    try:
        _, out = _amux("POST", "/api/board", data=json.dumps(access_card_body(user, ip)).encode(), timeout=10,
                       content_type="application/json")
        resp = json.loads(out or b"{}")
        if not resp.get("id"):     # a refusal comes back as JSON with error/code: read it, never assume
            raise ValueError(str(resp.get("code") or resp.get("error") or "no card id")[:80])
        update_allowlist(lambda d: [r.update(card=resp["id"]) for r in d.get("requests", [])
                                    if r.get("ip") == ip and r.get("user") == user] and None)
        log(event="allowlist", user=user, ip=ip, card=resp["id"], verdict="door_access_card_filed")
    except Exception as e:
        log(event="allowlist", user=user, ip=ip, verdict="door_access_card_failed", reason=str(e)[:120])


def close_access_card(card, how):
    if not card:
        return
    try:
        _, out = _amux("PATCH", "/api/board/" + urllib.parse.quote(card, safe=""), timeout=10,
                       content_type="application/json",
                       data=json.dumps({"status": "done", "evidence": "none: %s at /_door/allowlist; an allowlist "
                                                                      "change leaves no artifact" % how}).encode())
        resp = json.loads(out or b"{}")
        if resp.get("ok") is False or resp.get("error"):
            raise ValueError(str(resp.get("code") or resp.get("error"))[:80])
        log(event="allowlist", card=card, verdict="door_access_card_closed")
    except Exception as e:
        log(event="allowlist", card=card, verdict="door_access_card_close_failed", reason=str(e)[:120])


def take_request(ip, user):
    """Remove the waiting request for (ip, user) and return it, or None."""
    def take(data):
        reqs = data.get("requests", [])
        hit = next((r for r in reqs if r.get("ip") == ip and r.get("user") == user), None)
        if hit is None:
            return False
        reqs.remove(hit)
        return hit
    return update_allowlist(take) or None


def note_blocked(ip, uri, now=None):
    """One log line per address per minute, so a scanner cannot flood the journal."""
    now = now or time.time()
    if now - _blocked_logged.get(ip, 0) < 60:
        return
    if len(_blocked_logged) > 10000:
        _blocked_logged.clear()
    _blocked_logged[ip] = now
    log(event="check", ip=ip, uri=uri[:80], verdict="door_allowlist_blocked")


def seed_from_logs(paths, cloudflare_list="/etc/amux/cloudflare-ips"):
    """{ip: (user, requests)} for every address that made a signed-in request (Caddy's access log,
    door_user set, status under 400). Cloudflare's own addresses are skipped: before a host trusted
    CF-Connecting-IP, Caddy logged the proxy rather than the visitor."""
    import gzip
    import ipaddress
    try:
        cf = [ipaddress.ip_network(l.strip()) for l in open(cloudflare_list) if l.strip()]
    except OSError:
        cf = []
    seen = {}
    for p in paths:
        op = gzip.open if p.endswith(".gz") else open
        try:
            fh = op(p, "rt", errors="replace")
        except OSError:
            continue
        with fh:
            for line in fh:
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                user = d.get("door_user")
                if not user or d.get("status", 500) >= 400:
                    continue
                r = d.get("request") or {}
                ip = r.get("client_ip") or r.get("remote_ip") or ""
                try:
                    a = ipaddress.ip_address(ip)
                except ValueError:
                    continue
                if a.is_loopback or any(a in n for n in cf):
                    continue
                u, n = seen.get(ip, (user, 0))
                seen[ip] = (u, n + 1)
    return seen


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


def basic_user(header, ip=None):
    if not header or not header.startswith("Basic "):
        return None
    try:
        user, _, pw = base64.b64decode(header[6:]).decode().partition(":")
    except Exception:
        return None
    if ip and throttled(ip):
        return None
    if check_password(user, pw):
        return user
    if ip:
        note_failure(ip)
        log(event="basic", user=user, ip=ip, verdict="door_basic_refused")
    return None


# ── Trusted devices (Ethan, 2026-11-06: "doron is stuck again, maybe we need a better approach?").
# Doron was refused three more times overnight, each from a new cellular IPv6 address
# (2605:59ca:..., 2600:382:...): a phone's address changes with the network, so a list of addresses
# cannot hold it. A browser or app that signed in where the allowlist let it (a listed address, a
# first sign-in, an address the owner allowed) gets DEVICE_COOKIE, signed and kept for a year, and
# from then on that device passes the allowlist from any address. It is not a session: the person
# still signs in, and signing out keeps it. A stolen password on a new device is still refused and
# asks the owner, which is what the allowlist is for. The owner forgets an account's devices on
# /_door/allowlist (device_epoch), and changing the server secret forgets every device.
DEVICE_COOKIE = "amux_door_device"
DEVICE_MAX_AGE = 365 * 86400


def device_epoch(user, data=None):
    data = allowlist() if data is None else data
    return int((data.get("device_epoch") or {}).get(user, 0))


def device_cookie(user):
    v = make_cookie("device:%s:%d" % (user, device_epoch(user)), DEVICE_MAX_AGE)
    return ("Set-Cookie", "%s=%s; Path=/; Max-Age=%d; HttpOnly; Secure; SameSite=Lax" % (DEVICE_COOKIE, v, DEVICE_MAX_AGE))


def device_user(header):
    """The account whose sign-in made this a trusted device, or None (missing, forged, expired,
    forgotten by the owner, or an account that is gone)."""
    for part in (header or "").split(";"):
        k, _, v = part.strip().partition("=")
        if k != DEVICE_COOKIE or not v:
            continue
        try:
            name, exp, mac = v.rsplit(".", 2)
            exp = int(exp)
            tag, rest = name.split(":", 1)
            user, epoch = rest.rsplit(":", 1)
            epoch = int(epoch)
        except ValueError:
            return None
        if tag == "device" and exp > time.time() and hmac.compare_digest(sign(name, exp), mac) \
                and epoch == device_epoch(user) and (user in users() or user in members()):
            return user
        return None
    return None


def reachable(ip, cookies, data=None):
    """May this request pass the allowlist: off, a listed address, or a trusted device."""
    data = allowlist() if data is None else data
    return ip_allowed(ip, data) or bool(device_user(cookies))


def session_cookies(user, max_age=MAX_AGE, device=True):
    """Every caller but /_door/refresh issues these only after the allowlist let the sign-in through,
    so the device is trusted from then on (device=False where that is not known)."""
    v = make_cookie(user, max_age)
    return ([device_cookie(user)] if device else []) + [("Set-Cookie", "%s=%s; Path=/; Max-Age=%d; HttpOnly; Secure; SameSite=Lax" % (COOKIE, v, max_age)),
            ("Set-Cookie", "%s=%s; Path=%s; Max-Age=%d; HttpOnly; Secure; SameSite=None" % (VIEW_COOKIE, v, VIEW_PATH, max_age)),
            ("Set-Cookie", "%s=1; Path=/; Max-Age=%d; HttpOnly; Secure; SameSite=Lax" % (VIEW_MARK, max_age))]


def has_cookie(header, name):
    return any(p.strip().partition("=")[0] == name for p in (header or "").split(";"))


def safe_next(n):
    # Only a same-host path: never an open redirect.
    n = n or "/"
    return n if n.startswith("/") and not n.startswith("//") else "/"


# Where a signed-out person was going (a copied /f/<file> or /w/<worker> link), kept
# through amux Cloud's sign-in, which sends them back to /_door/enter without it.
NEXT_COOKIE = "amux_door_next"


def next_cookie(path):
    return ("Set-Cookie", "%s=%s; Path=/_door/; Max-Age=900; HttpOnly; Secure; SameSite=Lax"
            % (NEXT_COOKIE, urllib.parse.quote(safe_next(path), safe="/")))


def next_cookie_clear(header):
    """Spent once used: the next sign-in starts at home unless it too came from a link."""
    if not next_from_cookie(header):
        return []
    return [("Set-Cookie", "%s=; Path=/_door/; Max-Age=0; HttpOnly; Secure; SameSite=Lax" % NEXT_COOKIE)]


def next_from_cookie(header):
    for part in (header or "").split(";"):
        k, _, v = part.strip().partition("=")
        if k == NEXT_COOKIE and v:
            return safe_next(urllib.parse.unquote(v))
    return ""


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
.note{color:#d29922;font-size:13px;margin:12px 0 0}.chk{display:flex;gap:8px;align-items:flex-start;color:#8b949e;font-size:13px;margin-top:14px}
.chk input{width:auto;min-height:0;margin-top:2px}
</style></head><body>
<form method="post" action="/_door/login">
<h1>amux</h1><p>%(host)s</p>
<label for="u">Username</label><input id="u" name="username" autocomplete="username" autocapitalize="none" required value="%(user)s">
<label for="p">Password</label><input id="p" name="password" type="password" autocomplete="current-password" required>
<input type="hidden" name="next" value="%(next)s">%(allow)s
<button type="submit">Sign in</button>%(err)s
</form></body></html>"""

JOIN_PAGE = PAGE.replace('<form method="post" action="/_door/login">', '<form method="post" action="/_door/join">').replace(
    "<title>Sign in to amux</title>", "<title>Join amux</title>").replace(
    '<h1>amux</h1><p>%(host)s</p>', '<h1>Join %(host)s</h1><p>You were invited to this amux server. Choose the password you will sign in with.</p>').replace(
    '<label for="u">Username</label>', '<label for="u">Your email</label>').replace(
    'autocomplete="current-password" required>', 'autocomplete="new-password" minlength="10" required>'
    '<label for="p2">Password again</label><input id="p2" name="password2" type="password" autocomplete="new-password" required>').replace(
    '<input type="hidden" name="next" value="%(next)s">', '<input type="hidden" name="invite" value="%(next)s">').replace(
    '<button type="submit">Sign in</button>', '<button type="submit">Join</button>').replace('%(allow)s', '')


ALLOW_PAGE = """<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>Address allowlist</title>
<style>
body{margin:0;font:15px -apple-system,system-ui,sans-serif;background:#0d1117;color:#e6edf3;padding:24px max(16px,env(safe-area-inset-left))}
main{max-width:760px;margin:0 auto}h1{font-size:20px;margin:0 0 4px}p{color:#8b949e;margin:6px 0}
table{width:100%%;border-collapse:collapse;margin:16px 0;font-size:14px}td{border-top:1px solid #30363d;padding:8px 6px;vertical-align:middle}
.wrap{overflow-x:auto}form.inline{display:inline-flex;gap:8px;flex-wrap:wrap;align-items:center;margin:4px 0}
input{min-height:44px;padding:8px 10px;border-radius:8px;border:1px solid #30363d;background:#0d1117;color:#e6edf3;font-size:15px}
button{min-height:44px;padding:0 14px;border:0;border-radius:8px;background:#58a6ff;color:#0d1117;font-weight:600;font-size:14px}
button.sec{background:#21262d;color:#e6edf3;border:1px solid #30363d}.err{color:#f85149}.ok{color:#3fb950}
a{color:#58a6ff}
</style></head><body><main>
<h1>Address allowlist</h1><p>%(host)s</p>
%(err)s%(ok)s
<p><b>%(state)s</b></p>
<p>Your address: <code>%(you)s</code> (%(you_listed)s).</p>
<div>%(toggle)s</div>
<p>%(first_state)s</p><div>%(first)s</div>
%(requests)s<div class="wrap"><table>%(rows)s</table></div>
<div>%(add)s</div>
<h2>Trusted devices</h2><p>A phone, app or browser that signed in where this list let it keeps working from any
address, so a phone that changes network is not locked out. A new device still needs a listed address,
or your Allow above. Lost a device, or think a password leaked? Forget that account's devices: each one
then needs a listed address or your Allow again.</p><div>%(forget)s</div>
<p>Public links and the server's status page stay reachable from anywhere. Locked out? On the server:
<code>sudo python3 /opt/amux-door/door.py allowlist add &lt;address&gt;</code></p>
<p><a href="/">Back to amux</a></p>
</main></body></html>"""


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
        ip = client_ip(self.headers, self.client_address[0])
        al = allowlist()
        if al.get("enabled") and not ip_listed(ip, al):
            allow = ('<p class="note">Your address, %s, is not on this server\'s allowlist. If this is your '
                     'first sign-in, signing in adds it. Otherwise ask the owner to add it.</p>' % html.escape(ip))
        elif not os.path.exists(ALLOW_FILE):
            allow = ('<label class="chk"><input type="checkbox" name="lock" value="1"> Owner: only let this '
                     'address (%s) reach this server. You can add others later.</label>' % html.escape(ip))
        else:
            allow = ""
        body = PAGE % {"host": host, "next": html.escape(safe_next(nxt)), "user": html.escape(user), "allow": allow,
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
            # Another site (every amux Cloud server is under amux.io, so a customer's
            # neighbour is the "same site") may not change anything with this cookie.
            # amux refuses the same (api/cross_site.rs); this stops it at the door.
            site = (self.headers.get("Sec-Fetch-Site") or "").strip().lower()
            ws = (self.headers.get("Upgrade") or "").lower() == "websocket"
            if site in ("cross-site", "same-site") and (method not in ("GET", "HEAD", "OPTIONS") or ws) \
                    and not uri.startswith("/pub/"):
                log(event="check", verdict="door_cross_site_refused", method=method, site=site)
                return self.send(403, b'{"error":"cross-site request refused"}', [("Content-Type", "application/json")])
            cookies = self.headers.get("Cookie")
            ip = client_ip(self.headers, self.client_address[0])
            blocked = not reachable(ip, cookies)
            if blocked:
                note_blocked(ip, uri)
                if not (method == "GET" and "text/html" in self.headers.get("Accept", "")):
                    return self.send(403, json.dumps({"error": "this address is not on the server's allowlist",
                                                      "ip": ip}).encode(), [("Content-Type", "application/json")])
            user = None if blocked else cookie_user(cookies)
            if not user and not blocked and (self.headers.get("Authorization") or "").startswith("Basic "):
                wait = throttled(ip)
                if wait:
                    return self.send(429, b'{"error":"too many wrong passwords from this address; try again later"}',
                                     [("Content-Type", "application/json"), ("Retry-After", str(wait))])
                user = basic_user(self.headers.get("Authorization"), ip)
            if not user and not blocked and method == "GET" and uri.startswith(VIEW_PATH):
                user = cookie_user(cookies, VIEW_COOKIE)
            if user:
                tok = members().get(user)
                if user not in users() and not (tok and member_valid(tok)):
                    log(event="check", user=user, verdict="door_member_revoked")
                    user = None
            if user:
                # Signed in before the view cookie existed: one hop to add it.
                if (method == "GET" and "text/html" in self.headers.get("Accept", "")
                        and not (has_cookie(cookies, VIEW_MARK) and has_cookie(cookies, DEVICE_COOKIE))
                        and not uri.startswith(("/_door/", VIEW_PATH))):
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
                    keep = [next_cookie(uri)] if safe_next(uri) != "/" else []
                    if keep:
                        loc += "&next=" + urllib.parse.quote(safe_next(uri), safe="")
                    return self.send(302, headers=[("Location", loc), ("Cache-Control", "no-store")] + keep)
                loc = "/_door/login?next=" + urllib.parse.quote(safe_next(uri), safe="")
                return self.send(302, headers=[("Location", loc)])
            return self.send(401, b'{"error":"sign in required"}', [("Content-Type", "application/json"),
                             ("WWW-Authenticate", 'Basic realm="amux"')])
        if path.path == "/_door/enter":
            q = urllib.parse.parse_qs(path.query)
            ip = client_ip(self.headers, self.client_address[0])
            try:
                claims = verify_entry_token(q.get("t", [""])[0])
                email = claims["sub"].lower()
                if device_user(self.headers.get("Cookie")) != email and not first_signin_add(email, ip):
                    raise PermissionError("address not on the allowlist")
                if email in users():       # the server's owner (a standalone server's paying customer)
                    log(event="enter", user=email, ip=ip, role="owner", via=claims.get("via"), verdict="door_enter_ok")
                    note_signin(True)
                    nxt = safe_next(q.get("next", [""])[0] or next_from_cookie(self.headers.get("Cookie")))
                    return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(email)
                                     + next_cookie_clear(self.headers.get("Cookie")))
                known = members().get(email)
                if claims.get("invite") and not (known and member_valid(known)):
                    redeem_invite(email, claims["invite"])     # invites are single use; a member skips this
                tok = members().get(email)
                if not tok or not member_valid(tok):
                    raise ValueError("not a member of this server")
            except PermissionError:
                body = ("<!doctype html><meta charset=utf-8><title>Address not allowed</title><p>Your address, %s, "
                        "is not on this server's allowlist. Sign in from an address on the list and add it at "
                        "/_door/allowlist. If your password was right, the owner has been asked to let this address in; sign in again once they have.</p>"
                        % html.escape(ip)).encode()
                return self.send(403, body, [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])
            except Exception as e:
                log(event="enter", ip=ip, verdict="door_enter_refused", reason=str(e)[:120])
                body = ("<!doctype html><meta charset=utf-8><title>Link expired</title><p>This sign-in link has expired "
                        "or is not for this server. <a href='%s'>Sign in again</a></p>" %
                        html.escape(fleet().get("AMUX_CONTROL_URL", "/_door/login"))).encode()
                return self.send(401, body, [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])
            log(event="enter", user=email, ip=ip, role=claims.get("role"), verdict="door_enter_ok")
            note_signin(True)
            nxt = safe_next(q.get("next", [""])[0] or next_from_cookie(self.headers.get("Cookie")))
            return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] +
                             session_cookies(email, TOKEN_MAX_AGE) + next_cookie_clear(self.headers.get("Cookie")))
        if path.path == "/_door/ipcheck":
            # forward_auth for the apps origin (:8443), which has no sign-in of its own.
            ip = client_ip(self.headers, self.client_address[0])
            if reachable(ip, self.headers.get("Cookie")):
                return self.send(200)
            note_blocked(ip, self.headers.get("X-Forwarded-Uri", "/"))
            return self.send(403, b"This address is not on the server's allowlist.\n", [("Content-Type", "text/plain")])
        if path.path == "/_door/allowlist":
            return self.allowlist_page()
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
            # /_door/* is reachable from anywhere: a session cookie carried to an unlisted address must
            # not turn that device into a trusted one here.
            trust = reachable(client_ip(self.headers, self.client_address[0]), self.headers.get("Cookie"))
            return self.send(302, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(user, device=trust))
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
        if route not in ("/_door/login", "/_door/join", "/_door/allowlist"):
            return self.send(404)
        n = int(self.headers.get("Content-Length") or 0)
        form = urllib.parse.parse_qs(self.rfile.read(min(n, 65536)).decode(errors="replace"))
        if route == "/_door/join":
            return self.join(form)
        if route == "/_door/allowlist":
            return self.allowlist_post(form)
        user = (form.get("username") or [""])[0].strip()
        pw = (form.get("password") or [""])[0]
        nxt = safe_next((form.get("next") or ["/"])[0])
        ip = client_ip(self.headers, self.client_address[0])
        wait = throttled(ip)
        if wait:
            log(event="signin", user=user, ip=ip, verdict="door_signin_throttled_refused")
            return self.page(nxt, user, "Too many wrong passwords from this address. Try again in %d minutes." % (wait // 60 + 1))
        al = allowlist()
        # A device is trusted for the account that signed in on it, not for every account: another
        # account's password on it is a new sign-in from a new address, refused as one.
        device = device_user(self.headers.get("Cookie"))
        if device and device != user and al.get("enabled") and not ip_listed(ip, al):
            log(event="signin", user=user, ip=ip, device=device, verdict="door_signin_device_other_account")
        if al.get("enabled") and not ip_listed(ip, al) and device != user:
            ok = check_password(user, pw)
            if not (ok and first_signin_add(user, ip)):
                # Same cost whatever the password: a right one must not come back faster, and it
                # counts toward the throttle too, so this cannot be used to test passwords.
                note_failure(ip)
                time.sleep(1.0)
                log(event="signin", user=user, ip=ip, verdict="door_signin_allowlist_refused")
                note_signin(False)
                return self.page(nxt, user, "This address (%s) is not on this server's allowlist. "
                                            "If your password was right, the owner has been asked to let this address in; "
                                            "sign in again once they have." % ip)
        elif (form.get("lock") or [""])[0] == "1" and not os.path.exists(ALLOW_FILE) and check_password(user, pw):
            if user in admins({}):
                update_allowlist(lambda d: d.update(enabled=True, first_signin=True, admins=[user]))
                allow_add(ip, user, "owner_first_signin")
                log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_turned_on")
            else:
                log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_lock_not_owner")
        if check_password(user, pw):
            log(event="signin", user=user, ip=ip, verdict="door_signin_ok")
            note_signin(True)
            return self.send(303, headers=[("Location", nxt), ("Cache-Control", "no-store")] + session_cookies(user))
        log(event="signin", user=user, ip=ip, verdict="door_signin_refused")
        note_signin(False)
        note_failure(ip)
        time.sleep(1.0)  # slows guessing without a lockout a person could trip
        return self.page(nxt, user, "Wrong username or password.")


    # ── /_door/allowlist: the owner's page for the address allowlist (linked from Settings) ──
    def allowlist_admin(self):
        """(user, ip) for a signed-in admin on an allowed address, else None after answering."""
        ip = client_ip(self.headers, self.client_address[0])
        if not reachable(ip, self.headers.get("Cookie")):
            note_blocked(ip, "/_door/allowlist")
            self.send(403, b"This address is not on the server's allowlist.\n", [("Content-Type", "text/plain")])
            return None
        user = cookie_user(self.headers.get("Cookie"))
        if not user:
            self.send(302, headers=[("Location", "/_door/login?next=/_door/allowlist")])
            return None
        if user not in admins():
            log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_not_admin")
            self.send(403, b"Only the server's owner can change the address allowlist.\n", [("Content-Type", "text/plain")])
            return None
        if not ip_allowed(ip) and device_user(self.headers.get("Cookie")) != user:
            # Reached on another account's trusted device: the admin's own device or a listed address only.
            log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_device_other_account")
            self.send(403, b"This address is not on the server's allowlist.\n", [("Content-Type", "text/plain")])
            return None
        return user, ip

    def allowlist_page(self, err="", ok=""):
        who = self.allowlist_admin()
        if not who:
            return
        user, ip = who
        al = allowlist()
        on = bool(al.get("enabled"))
        first = al.get("first_signin", True)
        csrf = sign("allowlist:" + user, 0)
        def form(action, label, extra="", cls=""):
            return ('<form method="post" action="/_door/allowlist" class="inline"><input type="hidden" name="csrf" value="%s">'
                    '<input type="hidden" name="action" value="%s">%s<button class="%s" type="submit">%s</button></form>'
                    % (csrf, action, extra, cls, html.escape(label)))
        rows = "".join(
            "<tr><td><code>%s</code></td><td>%s</td><td>%s</td><td>%s</td><td>%s</td></tr>" % (
                html.escape(e.get("ip", "")), html.escape(e.get("user", "")),
                html.escape(e.get("how", "")), time.strftime("%Y-%m-%d", time.gmtime(e.get("added", 0))),
                form("remove", "Remove", '<input type="hidden" name="ip" value="%s">' % html.escape(e.get("ip", "")), "sec"))
            for e in al.get("entries", []))
        def req_row(r):
            hidden = ('<input type="hidden" name="ip" value="%s"><input type="hidden" name="user" value="%s">'
                      % (html.escape(r.get("ip", "")), html.escape(r.get("user", ""))))
            return "<tr><td><code>%s</code></td><td>%s</td><td>%s</td><td>%s %s</td></tr>" % (
                html.escape(r.get("ip", "")), html.escape(r.get("user", "")),
                "%s UTC, %d sign-in%s" % (time.strftime("%Y-%m-%d %H:%M", time.gmtime(r.get("last", 0))),
                                          r.get("count", 1), "" if r.get("count", 1) == 1 else "s"),
                form("allow_request", "Allow", hidden), form("dismiss_request", "Dismiss", hidden, "sec"))
        reqs = al.get("requests", [])
        requests = ('<h2>Waiting for you</h2><p>These signed in with the right password from an address that is not '
                    'on the list. Allow one only if you know it is them; if not, dismiss it and change that '
                    'password.</p><div class="wrap"><table>%s</table></div>' % "".join(map(req_row, reqs))) if reqs else ""
        accounts = sorted({e.get("user") for e in al.get("entries", []) + reqs if e.get("user")} | set(users()) | set(joined()))
        forget = "".join(form("forget_devices", "Forget %s's devices" % a,
                              '<input type="hidden" name="user" value="%s">' % html.escape(a), "sec") for a in accounts)
        body = ALLOW_PAGE % {
            "forget": forget or "<p>No accounts yet.</p>",
            "requests": requests,
            "host": html.escape(self.headers.get("X-Forwarded-Host") or self.headers.get("Host") or ""),
            "state": "On: only the addresses below reach this server." if on else
                     "Off: any address can reach the sign-in.",
            "you": html.escape(ip), "you_listed": "on the list" if ip_listed(ip, al) else "not on the list",
            "toggle": form("off", "Turn off", cls="sec") if on else form("on", "Turn on (adds your address)"),
            "first": form("first_off" if first else "first_on",
                          "Stop letting a first sign-in add its address" if first else "Let a first sign-in add its address",
                          cls="sec"),
            "first_state": "A person whose account has no address yet adds theirs by signing in." if first else
                           "Only you add addresses.",
            "rows": rows or '<tr><td colspan="5">No addresses yet.</td></tr>',
            "add": form("add", "Add", '<input name="ip" placeholder="203.0.113.7 or 203.0.113.0/24" required>'
                                      '<input name="note" placeholder="Label (optional)">'),
            "err": '<p class="err">%s</p>' % html.escape(err) if err else "",
            "ok": '<p class="ok">%s</p>' % html.escape(ok) if ok else "",
        }
        self.send(400 if err else 200, body.encode(), [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])

    def allowlist_post(self, form):
        who = self.allowlist_admin()
        if not who:
            return
        user, ip = who
        site = (self.headers.get("Sec-Fetch-Site") or "").strip().lower()
        given = (form.get("csrf") or [""])[0]
        if site in ("cross-site", "same-site") or not hmac.compare_digest(given, sign("allowlist:" + user, 0)):
            log(event="allowlist", user=user, ip=ip, verdict="door_allowlist_csrf_refused")
            return self.send(403, b"Refused: reload the page and try again.\n", [("Content-Type", "text/plain")])
        action = (form.get("action") or [""])[0]
        target = (form.get("ip") or [""])[0].strip()
        if action == "add":
            try:
                net = _net(target)
            except ValueError:
                return self.allowlist_page(err="%s is not an address or a range." % target)
            if net.prefixlen < (8 if net.version == 4 else 32):
                return self.allowlist_page(err="That range is too wide to be an allowlist entry.")
            added = allow_add(target, user, "owner", (form.get("note") or [""])[0])
            return self.allowlist_page(ok=("Added %s." % target) if added else "%s was already on the list." % target)
        if action == "forget_devices":
            who_for = (form.get("user") or [""])[0].strip()
            if not who_for:
                return self.allowlist_page(err="Name the account.")
            update_allowlist(lambda d: d.setdefault("device_epoch", {}).update({who_for: device_epoch(who_for, d) + 1}))
            log(event="allowlist", user=who_for, by=user, verdict="door_devices_forgotten")
            return self.allowlist_page(ok="Forgot every trusted device for %s." % who_for)
        if action in ("allow_request", "dismiss_request"):
            who_for = (form.get("user") or [""])[0].strip()
            req = take_request(target, who_for)
            if not req:
                return self.allowlist_page(err="That request is no longer waiting.")
            if action == "allow_request":
                allow_add(target, who_for, "owner_approved", "approved by %s" % user)
                how = "%s allowed %s for %s" % (user, target, who_for)
            else:
                how = "%s dismissed the request from %s for %s" % (user, target, who_for)
            log(event="allowlist", user=who_for, ip=target, by=user, verdict="door_access_" + action.split("_")[0] + "ed")
            threading.Thread(target=close_access_card, args=(req.get("card"), how), daemon=True).start()
            return self.allowlist_page(ok=("Allowed %s for %s. They can sign in now." % (target, who_for))
                                       if action == "allow_request" else "Dismissed %s." % target)
        if action == "remove":
            def remove(data):
                rest = [e for e in data["entries"] if e.get("ip") != target]
                if data.get("enabled") and not ip_listed(ip, {"entries": rest}):
                    return False
                data["entries"] = rest
                return True
            if not update_allowlist(remove):
                return self.allowlist_page(err="Removing %s would lock out your own address. Turn the allowlist off first, "
                                               "or add another entry that covers you." % target)
            log(event="allowlist", user=user, ip=target, verdict="door_allowlist_removed")
            return self.allowlist_page(ok="Removed %s." % target)
        if action in ("on", "off", "first_on", "first_off"):
            def toggle(data):
                if action == "on":
                    data.setdefault("admins", sorted(admins(data)))
                    if not ip_listed(ip, data):   # never switch it on with the owner outside
                        data["entries"].append({"ip": ip, "user": user, "how": "owner", "added": int(time.time()),
                                                "note": "turned the allowlist on"})
                    data["enabled"] = True
                elif action == "off":
                    data["enabled"] = False
                else:
                    data["first_signin"] = action == "first_on"
            update_allowlist(toggle)
            log(event="allowlist", user=user, ip=ip, action=action, verdict="door_allowlist_" + action)
            return self.allowlist_page(ok="Saved.")
        return self.allowlist_page(err="Unknown action.")

    def join(self, form):
        email = (form.get("username") or [""])[0].strip().lower()
        pw, pw2 = (form.get("password") or [""])[0], (form.get("password2") or [""])[0]
        invite = (form.get("invite") or [""])[0].strip()
        ip = client_ip(self.headers, self.client_address[0])
        if "@" not in email or not (3 <= len(email) <= 200) or any(c.isspace() for c in email):
            return self.join_page(invite, email, "Enter your email address.")
        if len(pw) < 10:
            return self.join_page(invite, email, "Use at least 10 characters.")
        if pw != pw2:
            return self.join_page(invite, email, "The two passwords differ.")
        if email in users() or email in joined():
            return self.join_page(invite, email, "That email already has an account here. Sign in instead.")
        al = allowlist()
        if al.get("enabled") and not ip_listed(ip, al) and not al.get("first_signin", True):
            log(event="join", user=email, ip=ip, verdict="door_join_allowlist_refused")
            return self.join_page(invite, email, "This address (%s) is not on this server's allowlist. Ask the owner to add it." % ip)
        try:
            redeem_invite(email, invite)
        except Exception as e:
            log(event="join", user=email, ip=ip, verdict="door_join_refused", reason=str(e)[:120])
            time.sleep(1.0)
            return self.join_page(invite, email, "This invite link is used or no longer valid. Ask the server owner for a new one.")
        save_joined(email, scrypt_hash(pw))
        first_signin_add(email, ip)
        log(event="join", user=email, ip=ip, verdict="door_join_ok")
        note_signin(True)
        return self.send(303, headers=[("Location", "/"), ("Cache-Control", "no-store")] + session_cookies(email))


class DoorServer(ThreadingHTTPServer):
    """The listen backlog. Every dashboard request passes through the door
    (Caddy forward_auth), and a page load fires ~100 at once. socketserver's
    default backlog is 5: the kernel dropped the rest, each dropped connection
    was retried a second later, and every call on a sandbox page load took
    1.2 s against a 5 ms server (TcpExtListenOverflows 21785, 2026-10-28:
    "the sandbox cloud is very slow")."""
    request_queue_size = 1024
    daemon_threads = True


def allowlist_cli(args):
    """door.py allowlist show | on | off | add <ip> [user] [note] | remove <ip> | admins <a,b>
                         | first-signin on|off | seed [--apply] <caddy access logs...>"""
    cmd = args[0] if args else "show"
    if cmd == "show":
        print(json.dumps(allowlist(), indent=1))
    elif cmd in ("on", "off"):
        n = update_allowlist(lambda d: (d.update(enabled=cmd == "on"), len(d["entries"]))[1])
        log(event="allowlist", action=cmd, via="cli", entries=n, verdict="door_allowlist_" + cmd)
    elif cmd == "add" and len(args) >= 2:
        print("added" if allow_add(args[1], args[2] if len(args) > 2 else "", "cli", " ".join(args[3:])) else "already listed")
    elif cmd == "remove" and len(args) == 2:
        update_allowlist(lambda d: d.update(entries=[e for e in d["entries"] if e.get("ip") != args[1]]))
        log(event="allowlist", ip=args[1], via="cli", verdict="door_allowlist_removed")
    elif cmd == "admins" and len(args) == 2:
        update_allowlist(lambda d: d.update(admins=[a for a in args[1].split(",") if a]))
    elif cmd == "first-signin" and len(args) == 2 and args[1] in ("on", "off"):
        update_allowlist(lambda d: d.update(first_signin=args[1] == "on"))
    elif cmd == "seed":
        apply = "--apply" in args
        seen = seed_from_logs([a for a in args[1:] if a != "--apply"])
        for ip, (user, n) in sorted(seen.items(), key=lambda kv: -kv[1][1]):
            print("%-40s %-24s %d signed-in requests" % (ip, user, n))
            if apply:
                allow_add(ip, user, "seed", "signed in before the allowlist")
        print("seed: %d address(es) made signed-in requests%s" % (len(seen), "; added" if apply else "; --apply adds them"))
    else:
        sys.exit(allowlist_cli.__doc__)


def main():
    if len(sys.argv) >= 3 and sys.argv[1] == "hash":
        print("%s %s" % (sys.argv[2], scrypt_hash(sys.stdin.readline().rstrip("\n"))))
        return
    if len(sys.argv) >= 2 and sys.argv[1] == "allowlist":
        return allowlist_cli(sys.argv[2:])
    if len(sys.argv) >= 2 and sys.argv[1] == "serve":
        log(event="start", port=PORT, users=len(users()), backlog=DoorServer.request_queue_size,
            verdict="door_started")
        DoorServer(("127.0.0.1", PORT), Door).serve_forever()
        return
    sys.exit(__doc__)


if __name__ == "__main__":
    main()
