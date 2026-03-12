#!/usr/bin/env python3
"""
amux cloud gateway — auth + per-user container orchestration
Verifies Clerk JWTs, starts/stops Docker containers per user, reverse-proxies requests.
"""

import os, json, time, sqlite3, subprocess, threading, urllib.request, urllib.error, base64
import hmac, hashlib
from http.server import HTTPServer, BaseHTTPRequestHandler, ThreadingHTTPServer

# ── Config ────────────────────────────────────────────────────────────────────
CLERK_PUBLISHABLE_KEY = os.environ["CLERK_PUBLISHABLE_KEY"]
CLERK_SECRET_KEY      = os.environ["CLERK_SECRET_KEY"]
R2_ACCESS_KEY         = os.environ["R2_ACCESS_KEY"]
R2_SECRET_KEY         = os.environ["R2_SECRET_KEY"]
CF_ACCOUNT_ID         = os.environ["CF_ACCOUNT_ID"]
R2_ENDPOINT           = f"https://{CF_ACCOUNT_ID}.r2.cloudflarestorage.com"
R2_BUCKET             = "amux-cloud-users"
COOKIE_SECRET         = os.environ.get("COOKIE_SECRET", "change-me")
ANTHROPIC_API_KEY     = os.environ.get("ANTHROPIC_API_KEY", "")
STRIPE_SECRET_KEY     = os.environ.get("STRIPE_SECRET_KEY", "")
STRIPE_WEBHOOK_SECRET = os.environ.get("STRIPE_WEBHOOK_SECRET", "")
STRIPE_PRO_PRICE_ID   = os.environ.get("STRIPE_PRO_PRICE_ID", "")

PORT          = int(os.environ.get("GATEWAY_PORT", "8080"))
COMPOSE_TPL   = os.path.join(os.path.dirname(__file__), "../docker/docker-compose.template.yml")
LITESTREAM_YML= os.path.join(os.path.dirname(__file__), "../litestream/litestream.yml")
DATA_DIR      = os.environ.get("AMUX_CLOUD_DATA", "/var/amux/users")
DB_PATH       = os.environ.get("GATEWAY_DB", "/var/amux/gateway.db")
IDLE_SECONDS  = int(os.environ.get("IDLE_TIMEOUT", "3600"))
PORT_BASE     = 9000
COOKIE_MAX_AGE = 86400 * 7  # 7 days
SIGNUP_PASSCODE = os.environ.get("SIGNUP_PASSCODE", "")

# ── Login HTML ─────────────────────────────────────────────────────────────────
_LOGIN_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>amux cloud</title>
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
      background: #0a0a0a; color: #e5e5e5;
      min-height: 100vh; display: flex; flex-direction: column;
      align-items: center; justify-content: center; gap: 28px;
    }
    .logo { font-size: 1.4rem; font-weight: 700; letter-spacing: -0.5px; color: #fff; }
    .logo span { color: #555; font-weight: 400; }
    #clerk-root { min-width: 320px; }
    #status { color: #aaa; font-size: 0.85rem; min-height: 1.2em; }
    .spinner {
      width: 18px; height: 18px;
      border: 2px solid #333; border-top-color: #aaa;
      border-radius: 50%; animation: spin 0.7s linear infinite;
      margin: 0 auto;
    }
    @keyframes spin { to { transform: rotate(360deg); } }
  </style>
</head>
<body>
  <div class="logo">amux <span>cloud</span></div>
  <div id="clerk-root"></div>
  <div id="passcode-root" style="display:none;text-align:center;">
    <div style="font-size:0.95rem;color:#ccc;margin-bottom:14px;">Enter access code to create your account</div>
    <input id="passcode-input" type="text" placeholder="Access code" autocomplete="off" spellcheck="false"
      style="padding:10px 14px;border-radius:7px;border:1px solid #333;background:#111;color:#e5e5e5;font-size:1rem;width:240px;text-align:center;">
    <div style="margin-top:10px;">
      <button id="passcode-btn" onclick="submitPasscode()"
        style="padding:9px 28px;border-radius:7px;border:none;background:#7c6fcd;color:#fff;font-size:0.95rem;font-weight:600;cursor:pointer;">Continue</button>
    </div>
    <div id="passcode-err" style="color:#f87171;font-size:0.82rem;margin-top:8px;min-height:1.2em;"></div>
  </div>
  <div id="status"></div>
  <script>
    const PK = '__CLERK_PK__';
    let exchanging = false;
    let pendingPasscode = '';
    const POST_LOGIN_REDIRECT = '__POST_LOGIN_REDIRECT__';

    function setStatus(msg) {
      document.getElementById('status').textContent = msg;
    }

    document.getElementById('passcode-input')?.addEventListener('keydown', e => {
      if (e.key === 'Enter') submitPasscode();
    });

    async function submitPasscode() {
      pendingPasscode = document.getElementById('passcode-input').value.trim();
      document.getElementById('passcode-err').textContent = '';
      if (!pendingPasscode) { document.getElementById('passcode-err').textContent = 'Please enter the access code'; return; }
      document.getElementById('passcode-btn').textContent = 'Checking\u2026';
      exchanging = false;
      await exchangeAndRedirect();
    }

    async function exchangeAndRedirect() {
      if (exchanging) return;
      exchanging = true;
      const clerkEl = document.getElementById('clerk-root');
      const pcEl = document.getElementById('passcode-root');
      if (pcEl.style.display === 'none') {
        clerkEl.innerHTML = '<div class="spinner"></div>';
      }
      setStatus('Starting your workspace\u2026');
      try {
        const token = await window.Clerk.session.getToken();
        const email = window.Clerk.user?.primaryEmailAddress?.emailAddress || '';
        const payload = { token, email };
        if (pendingPasscode) payload.passcode = pendingPasscode;
        const res = await fetch('/api/cloud-auth', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
        if (res.ok) {
          window.location.replace(POST_LOGIN_REDIRECT || '/');
        } else {
          const d = await res.json().catch(() => ({}));
          if (d.needs_passcode) {
            // New user — show passcode prompt
            clerkEl.style.display = 'none';
            pcEl.style.display = '';
            setStatus('');
            document.getElementById('passcode-input').focus();
            exchanging = false;
            return;
          }
          if (d.error === 'invalid_passcode') {
            document.getElementById('passcode-err').textContent = 'Incorrect access code';
            document.getElementById('passcode-btn').textContent = 'Continue';
            exchanging = false;
            return;
          }
          clerkEl.innerHTML = '';
          setStatus('Auth error: ' + (d.error || res.status));
          exchanging = false;
        }
      } catch (e) {
        document.getElementById('clerk-root').innerHTML = '';
        setStatus('Connection error \u2014 please refresh.');
        exchanging = false;
      }
    }

    const s = document.createElement('script');
    s.setAttribute('data-clerk-publishable-key', PK);
    s.src = 'https://cdn.jsdelivr.net/npm/@clerk/clerk-js@4/dist/clerk.browser.js';
    s.onerror = () => setStatus('Failed to load auth library.');
    s.onload = async () => {
      try {
        if (!window.Clerk) { setStatus('ERROR: Clerk not initialized'); return; }
        await window.Clerk.load();
        setStatus('');
        if (window.Clerk.user) { await exchangeAndRedirect(); return; }
        window.Clerk.mountSignIn(document.getElementById('clerk-root'), { routing: 'hash' });
        window.Clerk.addListener(({ user }) => {
          if (user && !exchanging) exchangeAndRedirect();
        });
      } catch(e) {
        setStatus('ERROR: ' + e.message);
      }
    };
    document.head.appendChild(s);
  </script>
</body>
</html>"""

# ── Invite accept HTML ─────────────────────────────────────────────────────────
_INVITE_ACCEPT_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Join workspace — amux</title>
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
      background: #0a0a0a; color: #e5e5e5;
      min-height: 100vh; display: flex; align-items: center; justify-content: center; }
    .card { background: #1a1a1a; border: 1px solid #333; border-radius: 12px;
      padding: 40px; max-width: 420px; width: 90%; text-align: center; }
    h1 { font-size: 1.3rem; margin-bottom: 8px; }
    .owner { color: #a78bfa; font-weight: 600; font-size: 1.1rem; margin-bottom: 14px; }
    p { color: #888; font-size: 0.88rem; margin-bottom: 28px; line-height: 1.5; }
    .btn { display: inline-block; background: #a78bfa; color: #000; border: none;
      border-radius: 8px; padding: 12px 32px; font-size: 1rem; font-weight: 600;
      cursor: pointer; width: 100%; }
    .btn:hover { background: #c4b5fd; }
    .note { font-size: 0.72rem; color: #555; margin-top: 14px; }
    form { margin: 0; }
  </style>
</head>
<body>
  <div class="card">
    <h1>You've been invited to</h1>
    <div class="owner">__OWNER_EMAIL__</div>
    <p>Accept to view their sessions, board, and files.<br>
       You can switch back to your own workspace anytime from Settings.</p>
    <form action="/api/gateway/invite/__TOKEN__/accept" method="POST">
      <button class="btn" type="submit">Accept Invitation</button>
    </form>
    <div class="note">This invite expires in 7 days.</div>
  </div>
</body>
</html>"""

# ── DB ────────────────────────────────────────────────────────────────────────
_db_lock = threading.Lock()

def get_db():
    conn = sqlite3.connect(DB_PATH, check_same_thread=False)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS users (
            id          TEXT PRIMARY KEY,
            email       TEXT,
            plan        TEXT NOT NULL DEFAULT 'free',
            port        INTEGER UNIQUE,
            created_at  INTEGER NOT NULL,
            last_seen   INTEGER NOT NULL,
            stripe_customer_id TEXT,
            stripe_subscription_id TEXT
        );
    """)
    # Migrate: add stripe columns if missing
    try:
        conn.execute("SELECT stripe_customer_id FROM users LIMIT 1")
    except sqlite3.OperationalError:
        conn.execute("ALTER TABLE users ADD COLUMN stripe_customer_id TEXT")
        conn.execute("ALTER TABLE users ADD COLUMN stripe_subscription_id TEXT")
        conn.commit()
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS waitlist (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL UNIQUE,
            ts    INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS org_invites (
            token       TEXT PRIMARY KEY,
            owner_id    TEXT NOT NULL,
            email       TEXT,
            created_at  INTEGER NOT NULL,
            expires_at  INTEGER NOT NULL,
            used_at     INTEGER,
            used_by     TEXT
        );
        CREATE TABLE IF NOT EXISTS org_members (
            owner_id    TEXT NOT NULL,
            member_id   TEXT NOT NULL,
            member_email TEXT,
            joined_at   INTEGER NOT NULL,
            PRIMARY KEY (owner_id, member_id)
        );
    """)
    conn.commit()
    return conn

# ── Port allocation ────────────────────────────────────────────────────────────
def alloc_port(db):
    used = {r[0] for r in db.execute("SELECT port FROM users WHERE port IS NOT NULL")}
    p = PORT_BASE
    while p in used:
        p += 1
    return p

# ── Docker helpers ─────────────────────────────────────────────────────────────
def _compose_dir(user_id):
    d = os.path.join(DATA_DIR, user_id)
    os.makedirs(d, exist_ok=True)
    return d

def _write_compose(user_id, port):
    tpl = open(COMPOSE_TPL).read()
    yml = open(LITESTREAM_YML).read()
    compose = (tpl
        .replace("${USER_ID}", user_id)
        .replace("${USER_PORT}", str(port))
        .replace("${CF_ACCOUNT_ID}", CF_ACCOUNT_ID)
        .replace("${R2_ENDPOINT}", R2_ENDPOINT)
        .replace("${R2_ACCESS_KEY}", R2_ACCESS_KEY)
        .replace("${R2_SECRET_KEY}", R2_SECRET_KEY))
    d = _compose_dir(user_id)
    open(os.path.join(d, "docker-compose.yml"), "w").write(compose)
    open(os.path.join(d, "litestream.yml"), "w").write(
        yml.replace("${USER_ID}", user_id))

def container_running(user_id):
    r = subprocess.run(
        ["docker", "inspect", "-f", "{{.State.Running}}", f"amux-user-{user_id}"],
        capture_output=True, text=True)
    return r.stdout.strip() == "true"

def container_healthy(user_id):
    r = subprocess.run(
        ["docker", "inspect", "-f", "{{.State.Health.Status}}", f"amux-user-{user_id}"],
        capture_output=True, text=True)
    return r.stdout.strip() == "healthy"

def _restore_user_files(user_id):
    """Restore ~/.amux/ flat files from R2 on every startup (safe: sync only adds/updates)."""
    vol = f"amux-data-{user_id}"
    r2_prefix = f"s3://{R2_BUCKET}/users/{user_id}/files/"
    subprocess.run(
        ["docker", "run", "--rm",
         "-v", f"{vol}:/root/.amux",
         "-e", f"AWS_ACCESS_KEY_ID={R2_ACCESS_KEY}",
         "-e", f"AWS_SECRET_ACCESS_KEY={R2_SECRET_KEY}",
         "amazon/aws-cli:latest",
         "aws", "s3", "sync", r2_prefix, "/root/.amux/",
         "--endpoint-url", R2_ENDPOINT,
         "--exclude", "amux.db", "--exclude", "amux.db-shm", "--exclude", "amux.db-wal",
         "--quiet"],
        capture_output=True)

def start_container(user_id, port):
    _write_compose(user_id, port)
    _restore_user_files(user_id)
    d = _compose_dir(user_id)
    subprocess.run(["docker", "compose", "up", "-d"], cwd=d,
                   capture_output=True, check=True)
    # Wait for healthy (amux-server.py ready), not just running
    for _ in range(40):
        time.sleep(1)
        if container_healthy(user_id):
            break

def stop_container(user_id):
    d = _compose_dir(user_id)
    subprocess.run(["docker", "compose", "stop"], cwd=d, capture_output=True)

def _migrate_and_stop_member_container(member_id, owner_id):
    """Migrate session/memory files from member's container to owner's, then stop member's."""
    member_ctr = f"amux-user-{member_id}"
    owner_ctr = f"amux-user-{owner_id}"
    # Check if member container exists and has data
    r = subprocess.run(["docker", "inspect", member_ctr], capture_output=True)
    if r.returncode != 0:
        return  # no container to migrate from
    # Ensure owner container is running
    if not container_running(owner_id):
        return
    # Copy session files
    tmp = f"/tmp/amux-migrate-{member_id}"
    os.makedirs(tmp, exist_ok=True)
    for subdir in ["sessions", "memory"]:
        src = f"{member_ctr}:/root/.amux/{subdir}/."
        dst = os.path.join(tmp, subdir)
        os.makedirs(dst, exist_ok=True)
        subprocess.run(["docker", "cp", src, dst], capture_output=True)
        # Copy into owner container
        for fname in os.listdir(dst):
            fpath = os.path.join(dst, fname)
            if os.path.isfile(fpath) and not fname.startswith("_global"):
                subprocess.run(
                    ["docker", "cp", fpath, f"{owner_ctr}:/root/.amux/{subdir}/{fname}"],
                    capture_output=True)
    # Clean up temp
    import shutil
    shutil.rmtree(tmp, ignore_errors=True)
    # Stop member's container stack
    stop_container(member_id)
    print(f"[invite] migrated {member_id} → {owner_id} and stopped member container", flush=True)

# ── Session cookie ─────────────────────────────────────────────────────────────
def _make_cookie(user_id):
    ts = int(time.time())
    payload = f"{user_id}|{ts}"
    sig = hmac.new(COOKIE_SECRET.encode(), payload.encode(), hashlib.sha256).hexdigest()
    return f"{payload}|{sig}"

def _verify_cookie(val):
    try:
        last = val.rfind("|")
        if last == -1:
            raise ValueError("bad format")
        payload, sig = val[:last], val[last+1:]
        expected = hmac.new(COOKIE_SECRET.encode(), payload.encode(), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(sig, expected):
            raise ValueError("bad signature")
        parts = payload.split("|")
        if len(parts) != 2:
            raise ValueError("bad payload")
        uid, ts = parts
        if int(time.time()) - int(ts) > COOKIE_MAX_AGE:
            raise ValueError("expired")
        return uid
    except ValueError:
        raise
    except Exception:
        raise ValueError("invalid cookie")

def _parse_cookies(header):
    cookies = {}
    if not header:
        return cookies
    for part in header.split(";"):
        part = part.strip()
        if "=" in part:
            k, v = part.split("=", 1)
            cookies[k.strip()] = v.strip()
    return cookies

# ── Clerk JWT verification ─────────────────────────────────────────────────────
_jwks_cache = {"keys": None, "ts": 0}
_jwks_lock  = threading.Lock()

def _get_jwks():
    with _jwks_lock:
        if _jwks_cache["keys"] and time.time() - _jwks_cache["ts"] < 3600:
            return _jwks_cache["keys"]
    raw = CLERK_PUBLISHABLE_KEY.split("_", 2)[2]
    raw += "=" * (-len(raw) % 4)
    domain = base64.b64decode(raw).decode().strip("$")
    url = f"https://{domain}/.well-known/jwks.json"
    resp = urllib.request.urlopen(url, timeout=5)
    keys = json.loads(resp.read())["keys"]
    with _jwks_lock:
        _jwks_cache["keys"] = keys
        _jwks_cache["ts"] = time.time()
    return keys

def verify_clerk_token(token):
    """Verify a Clerk session JWT. Returns (user_id, email) or raises."""
    import jwt as pyjwt
    keys = _get_jwks()
    header = pyjwt.get_unverified_header(token)
    kid = header.get("kid")
    key = next((k for k in keys if k["kid"] == kid), None)
    if not key:
        raise ValueError("unknown kid")
    public_key = pyjwt.algorithms.RSAAlgorithm.from_jwk(json.dumps(key))
    payload = pyjwt.decode(token, public_key, algorithms=["RS256"],
                           options={"verify_aud": False})
    return payload["sub"], payload.get("email", "")

_clerk_email_cache = {}  # user_id -> email, simple in-memory cache

def _clerk_get_email(user_id):
    """Fetch user email from Clerk API. Returns '' on failure."""
    if user_id in _clerk_email_cache:
        return _clerk_email_cache[user_id]
    try:
        req = urllib.request.Request(
            f"https://api.clerk.com/v1/users/{user_id}",
            headers={"Authorization": f"Bearer {CLERK_SECRET_KEY}"}
        )
        resp = urllib.request.urlopen(req, timeout=5)
        data = json.loads(resp.read())
        addrs = data.get("email_addresses", [])
        primary_id = data.get("primary_email_address_id", "")
        email = ""
        for a in addrs:
            if a.get("id") == primary_id:
                email = a.get("email_address", "")
                break
        if not email and addrs:
            email = addrs[0].get("email_address", "")
        _clerk_email_cache[user_id] = email
        return email
    except Exception:
        return ""

# ── Idle reaper ────────────────────────────────────────────────────────────────
def _reaper():
    while True:
        time.sleep(60)
        try:
            db = get_db()
            cutoff = int(time.time()) - IDLE_SECONDS
            # Exempt org members (they use owner's container) and org owners
            # whose members are still active — member activity keeps the
            # shared container alive even when the owner hasn't visited.
            org_member_ids = {r["member_id"] for r in
                db.execute("SELECT member_id FROM org_members").fetchall()}
            active_owner_ids = {r["owner_id"] for r in
                db.execute(
                    "SELECT DISTINCT m.owner_id FROM org_members m "
                    "JOIN users u ON m.member_id = u.id WHERE u.last_seen >= ?",
                    (cutoff,)).fetchall()}
            stale = db.execute(
                "SELECT id FROM users WHERE last_seen < ? AND plan = 'free'",
                (cutoff,)).fetchall()
            for row in stale:
                uid = row["id"]
                if uid in org_member_ids or uid in active_owner_ids:
                    continue
                if container_running(uid):
                    print(f"[reaper] stopping idle container for {uid}")
                    stop_container(uid)
        except Exception as e:
            print(f"[reaper] error: {e}")

# Reaper disabled — not needed with current user count
# threading.Thread(target=_reaper, daemon=True).start()

# ── Share token resolver (caches token→port for 60s) ──────────────────────────
_share_cache = {}  # token → (port, expiry_time)
_share_cache_lock = threading.Lock()

def _resolve_share_token(token: str) -> int | None:
    """Find which container owns a share token. Returns port or None."""
    now = time.time()
    with _share_cache_lock:
        cached = _share_cache.get(token)
        if cached and cached[1] > now:
            return cached[0]
    # Query all running containers
    db = get_db()
    rows = db.execute("SELECT id, port FROM users WHERE port IS NOT NULL").fetchall()
    for row in rows:
        port = row["port"]
        try:
            resp = urllib.request.urlopen(
                f"http://127.0.0.1:{port}/api/share/{token}/info", timeout=3)
            if resp.status == 200:
                with _share_cache_lock:
                    _share_cache[token] = (port, now + 60)
                return port
        except Exception:
            continue
    return None


# ── Proxy helper ───────────────────────────────────────────────────────────────
def proxy(handler, port, path, qs, user_email="", user_id=None):
    url = f"http://127.0.0.1:{port}{path}"
    if qs:
        url += "?" + qs
    length = int(handler.headers.get("Content-Length", 0))
    body = handler.rfile.read(length) if length else None
    # Strip auth headers so container doesn't see them
    skip = {"host", "content-length", "authorization", "cookie"}
    fwd = {k: v for k, v in handler.headers.items() if k.lower() not in skip}
    if user_email:
        fwd["X-Amux-User-Email"] = user_email
    is_sse = handler.headers.get("Accept", "") == "text/event-stream"
    req = urllib.request.Request(url, data=body, method=handler.command, headers=fwd)
    try:
        resp = urllib.request.urlopen(req, timeout=None if is_sse else 60)
        handler.send_response(resp.status)
        for k, v in resp.headers.items():
            if k.lower() not in ("transfer-encoding",):
                handler.send_header(k, v)
        handler.end_headers()
        if is_sse:
            # Stream SSE chunk-by-chunk; refresh last_seen every 60s so
            # the reaper doesn't kill containers with active SSE connections.
            last_touch = time.time()
            try:
                while True:
                    chunk = resp.read(4096)
                    if not chunk:
                        break
                    handler.wfile.write(chunk)
                    handler.wfile.flush()
                    if user_id and time.time() - last_touch > 60:
                        try:
                            db = get_db()
                            db.execute("UPDATE users SET last_seen=? WHERE id=?",
                                       (int(time.time()), user_id))
                            db.commit()
                        except Exception:
                            pass
                        last_touch = time.time()
            except (BrokenPipeError, ConnectionResetError):
                pass
        else:
            try:
                handler.wfile.write(resp.read())
            except (BrokenPipeError, ConnectionResetError):
                pass
    except urllib.error.HTTPError as e:
        try:
            handler.send_response(e.code)
            handler.end_headers()
            handler.wfile.write(e.read())
        except (BrokenPipeError, ConnectionResetError):
            pass
    except urllib.error.URLError as e:
        try:
            handler.send_response(502)
            handler.end_headers()
            handler.wfile.write(f"Bad Gateway: {e.reason}".encode())
        except (BrokenPipeError, ConnectionResetError):
            pass

# ── Request handler ────────────────────────────────────────────────────────────
class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        import sys
        sys.stderr.write(f"[gateway] {self.client_address[0]} {fmt % args}\n")
        sys.stderr.flush()

    def _json(self, d, code=200):
        body = json.dumps(d).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body)

    def _html(self, body_str, code=200):
        body = body_str.encode() if isinstance(body_str, str) else body_str
        self.send_response(code)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _redirect(self, location, extra_cookies=None):
        self.send_response(302)
        self.send_header("Location", location)
        for cookie in (extra_cookies or []):
            self.send_header("Set-Cookie", cookie)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _serve_login(self, post_login_redirect="/"):
        html = (_LOGIN_HTML
                .replace("__CLERK_PK__", CLERK_PUBLISHABLE_KEY)
                .replace("__POST_LOGIN_REDIRECT__", post_login_redirect))
        self._html(html)

    def _serve_invite_accept(self, token, owner_email):
        html = (_INVITE_ACCEPT_HTML
                .replace("__OWNER_EMAIL__", owner_email or "someone")
                .replace("__TOKEN__", token))
        self._html(html)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        if not length:
            return {}
        raw = self.rfile.read(length)
        ct = self.headers.get("Content-Type", "")
        if "json" in ct:
            try:
                return json.loads(raw)
            except Exception:
                return {}
        return {}  # form posts: token is in URL, no fields needed

    def _is_https(self):
        return self.headers.get("X-Forwarded-Proto", "") == "https"

    def _base_url(self):
        scheme = "https" if self._is_https() else "http"
        host = self.headers.get("Host", f"localhost:{PORT}")
        return f"{scheme}://{host}"

    def _secure_cookie_flags(self):
        return "; Secure" if self._is_https() else ""

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Authorization, Content-Type")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _handle(self):
        from urllib.parse import urlparse
        parsed = urlparse(self.path)
        path = parsed.path
        qs   = parsed.query

        # ── Public: static assets (favicon — no auth required) ──
        _ICON_PATHS = {
            "/icon.svg": ("image/svg+xml", "/opt/amux-cloud/app/icon.svg"),
            "/icon.png": ("image/png",     "/opt/amux-cloud/app/icon.png"),
            "/icon-192.png": ("image/png", "/opt/amux-cloud/app/icon-192.png"),
            "/icon-512.png": ("image/png", "/opt/amux-cloud/app/icon-512.png"),
            "/favicon.ico": ("image/png",  "/opt/amux-cloud/app/icon.png"),
        }
        if path in _ICON_PATHS and self.command == "GET":
            ct, fpath = _ICON_PATHS[path]
            try:
                data = open(fpath, "rb").read()
                self.send_response(200)
                self.send_header("Content-Type", ct)
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Cache-Control", "public, max-age=86400")
                self.end_headers()
                self.wfile.write(data)
            except FileNotFoundError:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
            return

        # ── Public: shared session links — /s/<token> and /api/share/<token>/* ──
        if path.startswith("/s/") or path.startswith("/api/share/"):
            # Extract token: /s/<token> or /api/share/<token>/...
            if path.startswith("/s/"):
                token = path[3:].split("/")[0]
            else:
                token = path[len("/api/share/"):].split("/")[0]
            if token:
                # Find which user's container has this share token by querying
                # each running container. Cache result for 60s to avoid repeated lookups.
                target_port = _resolve_share_token(token)
                if target_port:
                    return proxy(self, target_port, path, qs)
            # Fall through to 404 if token not found
            return self._json({"error": "share link not found"}, 404)

        # ── Public: waitlist signup ──
        if path == "/api/waitlist" and self.command == "POST":
            length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(length)) if length else {}
            email = body.get("email", "").strip().lower()
            if not email or "@" not in email:
                return self._json({"error": "invalid email"}, 400)
            db = get_db()
            try:
                db.execute("INSERT INTO waitlist (email, ts) VALUES (?,?)",
                           (email, int(time.time())))
                db.commit()
                return self._json({"ok": True})
            except sqlite3.IntegrityError:
                return self._json({"ok": True, "already": True})

        # ── Public: Stripe webhook (signature-verified, no auth cookie needed) ──
        if path == "/api/stripe/webhook" and self.command == "POST":
            if not STRIPE_SECRET_KEY:
                return self._json({"error": "stripe not configured"}, 503)
            length = int(self.headers.get("Content-Length", 0))
            payload = self.rfile.read(length)
            sig = self.headers.get("Stripe-Signature", "")
            try:
                import stripe
                stripe.api_key = STRIPE_SECRET_KEY
                event = stripe.Webhook.construct_event(payload, sig, STRIPE_WEBHOOK_SECRET)
            except Exception as e:
                return self._json({"error": f"webhook verify failed: {e}"}, 400)
            db = get_db()
            etype = event["type"]
            obj = event["data"]["object"]
            if etype == "checkout.session.completed":
                cust_id = obj.get("customer")
                clerk_uid = obj.get("client_reference_id")
                sub_id = obj.get("subscription")
                if clerk_uid and cust_id:
                    with _db_lock:
                        db.execute(
                            "UPDATE users SET plan='pro', stripe_customer_id=?, stripe_subscription_id=? WHERE id=?",
                            (cust_id, sub_id, clerk_uid))
                        db.commit()
                    print(f"[stripe] activated pro for {clerk_uid} cust={cust_id}", flush=True)
            elif etype == "invoice.paid":
                cust_id = obj.get("customer")
                if cust_id:
                    with _db_lock:
                        db.execute("UPDATE users SET plan='pro' WHERE stripe_customer_id=?", (cust_id,))
                        db.commit()
            elif etype in ("customer.subscription.deleted", "customer.subscription.paused"):
                cust_id = obj.get("customer")
                if cust_id:
                    with _db_lock:
                        db.execute(
                            "UPDATE users SET plan='free', stripe_subscription_id=NULL WHERE stripe_customer_id=?",
                            (cust_id,))
                        db.commit()
                    print(f"[stripe] downgraded {cust_id} to free", flush=True)
            elif etype == "invoice.payment_failed":
                cust_id = obj.get("customer")
                print(f"[stripe] payment failed for {cust_id}", flush=True)
            return self._json({"ok": True})

        # ── Public: exchange Clerk JWT for session cookie ──
        if path == "/api/cloud-logout" and self.command in ("GET", "POST"):
            sec = self._secure_cookie_flags()
            self.send_response(302)
            self.send_header("Location", "/")
            self.send_header("Set-Cookie",
                f"amux_session=; HttpOnly{sec}; SameSite=Lax; Max-Age=0; Path=/")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        if path == "/api/cloud-auth" and self.command == "POST":
            length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(length)) if length else {}
            token = body.get("token", "")
            client_email = body.get("email", "").strip()  # email sent from Clerk.js
            try:
                user_id, email = verify_clerk_token(token)
            except Exception as e:
                return self._json({"error": f"invalid token: {e}"}, 401)
            # Prefer email from client (Clerk.js), then JWT, then Clerk API
            if not email:
                email = client_email or _clerk_get_email(user_id)
            db = get_db()
            now = int(time.time())
            with _db_lock:
                row = db.execute("SELECT id FROM users WHERE id=?", (user_id,)).fetchone()
                if not row:
                    # New user — require passcode
                    passcode = body.get("passcode", "").strip()
                    if passcode != SIGNUP_PASSCODE:
                        return self._json({"error": "invalid_passcode", "needs_passcode": True}, 403)
                    port = alloc_port(db)
                    db.execute(
                        "INSERT INTO users (id, email, plan, port, created_at, last_seen) VALUES (?,?,?,?,?,?)",
                        (user_id, email, "free", port, now, now))
                    db.commit()
                    print(f"[signup] new user {email} ({user_id}) with passcode", flush=True)
                else:
                    db.execute("UPDATE users SET last_seen=?, email=? WHERE id=?",
                               (now, email, user_id))
                    db.commit()
            cookie_val = _make_cookie(user_id)
            resp_body = json.dumps({"ok": True}).encode()
            sec = self._secure_cookie_flags()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(resp_body)))
            self.send_header("Set-Cookie",
                f"amux_session={cookie_val}; HttpOnly{sec}; SameSite=Lax; "
                f"Max-Age={COOKIE_MAX_AGE}; Path=/")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(resp_body)
            return

        # ── Unauthenticated invite: serve login with post-login redirect to invite page ──
        if path.startswith("/invite/") and self.command == "GET":
            cookies = _parse_cookies(self.headers.get("Cookie", ""))
            if not cookies.get("amux_session"):
                accept = self.headers.get("Accept", "")
                if "text/html" in accept:
                    invite_token = path[len("/invite/"):]
                    return self._serve_login(post_login_redirect=f"/invite/{invite_token}")

        # ── Resolve user: Bearer token OR session cookie ──
        user_id = None
        email   = ""
        auth = self.headers.get("Authorization", "")
        if auth.startswith("Bearer "):
            try:
                user_id, email = verify_clerk_token(auth[7:])
            except Exception as e:
                print(f"[auth] Bearer verify failed for {path}: {e}", flush=True)
                return self._json({"error": f"invalid token: {e}"}, 401)
        else:
            cookies = _parse_cookies(self.headers.get("Cookie", ""))
            session_val = cookies.get("amux_session", "")
            if session_val:
                try:
                    user_id = _verify_cookie(session_val)
                except ValueError as ve:
                    print(f"[auth] Cookie verify failed for {path}: {ve} cookie_len={len(session_val)}", flush=True)
                    accept = self.headers.get("Accept", "")
                    if "text/html" in accept or not path.startswith("/api/"):
                        return self._serve_login()
                    return self._json({"error": "session expired"}, 401)
            else:
                accept = self.headers.get("Accept", "")
                cookie_header = self.headers.get("Cookie", "")
                print(f"[auth] No amux_session cookie for {path} accept={accept[:40]} cookies={cookie_header[:60]}", flush=True)
                if "text/html" in accept or not path.startswith("/api/"):
                    return self._serve_login()
                return self._json({"error": "unauthorized"}, 401)

        # Upsert user
        db = get_db()
        now = int(time.time())
        with _db_lock:
            row = db.execute("SELECT * FROM users WHERE id=?", (user_id,)).fetchone()
            if not row:
                port = alloc_port(db)
                db.execute(
                    "INSERT INTO users (id, email, plan, port, created_at, last_seen) VALUES (?,?,?,?,?,?)",
                    (user_id, email, "free", port, now, now))
                db.commit()
                row = db.execute("SELECT * FROM users WHERE id=?", (user_id,)).fetchone()
            else:
                db.execute("UPDATE users SET last_seen=? WHERE id=?", (now, user_id))
                db.commit()

        port = row["port"]
        user_email = row["email"] or email
        # If we still don't have an email, fetch from Clerk API and persist it
        if not user_email:
            user_email = _clerk_get_email(user_id)
            if user_email:
                with _db_lock:
                    db.execute("UPDATE users SET email=? WHERE id=?", (user_email, user_id))
                    db.commit()

        # ── Gateway-level org/invite interceptors ─────────────────────────────

        # GET /invite/<token> while authenticated → show accept page
        if path.startswith("/invite/") and self.command == "GET":
            tok = path[len("/invite/"):]
            inv = db.execute(
                "SELECT owner_id FROM org_invites WHERE token=? AND used_at IS NULL AND expires_at > ?",
                (tok, now)
            ).fetchone()
            if not inv:
                return self._html("<html><body style='font-family:sans-serif;background:#0a0a0a;color:#e5e5e5;display:flex;align-items:center;justify-content:center;min-height:100vh;'><div style='text-align:center'><h2 style='color:#f87171'>Invite expired or invalid</h2><p style='color:#888;margin-top:8px'>This invite link is no longer valid.</p></div></body></html>", 410)
            owner = db.execute("SELECT email FROM users WHERE id=?", (inv["owner_id"],)).fetchone()
            owner_email = owner["email"] if owner else "someone"
            if inv["owner_id"] == user_id:
                return self._html("<html><body style='font-family:sans-serif;background:#0a0a0a;color:#e5e5e5;display:flex;align-items:center;justify-content:center;min-height:100vh;'><div style='text-align:center'><h2>That's your own invite link!</h2><p style='color:#888;margin-top:8px'>Share it with someone else.</p></div></body></html>")
            return self._serve_invite_accept(tok, owner_email)

        # POST /api/gateway/invite/<token>/accept → accept invite, set amux_org, redirect
        if path.startswith("/api/gateway/invite/") and path.endswith("/accept"):
            tok = path[len("/api/gateway/invite/"):-len("/accept")]
            inv = db.execute(
                "SELECT owner_id FROM org_invites WHERE token=? AND used_at IS NULL AND expires_at > ?",
                (tok, now)
            ).fetchone()
            if not inv:
                return self._json({"error": "invalid or expired invite"}, 410)
            owner_id = inv["owner_id"]
            db.execute("UPDATE org_invites SET used_at=?, used_by=? WHERE token=?",
                       (now, user_id, tok))
            db.execute(
                "INSERT OR IGNORE INTO org_members (owner_id, member_id, member_email, joined_at) "
                "VALUES (?,?,?,?)", (owner_id, user_id, user_email, now))
            db.commit()
            # Migrate member's session data into owner's container, then stop member's
            try:
                _migrate_and_stop_member_container(user_id, owner_id)
            except Exception as e:
                print(f"[invite] migration error for {user_id} → {owner_id}: {e}", flush=True)
            sec = self._secure_cookie_flags()
            return self._redirect(
                self._base_url() + "/",
                extra_cookies=[f"amux_org={owner_id}; HttpOnly{sec}; SameSite=Lax; Path=/"]
            )

        # POST /api/org/invites → create gateway-level invite (intercepted before container)
        if path == "/api/org/invites" and self.command == "POST":
            import secrets as _sec
            body = self._read_body()
            tok = _sec.token_urlsafe(24)
            expires = now + 7 * 86400
            db.execute(
                "INSERT INTO org_invites (token, owner_id, email, created_at, expires_at) "
                "VALUES (?,?,?,?,?)",
                (tok, user_id, body.get("email") or None, now, expires)
            )
            db.commit()
            url = f"{self._base_url()}/invite/{tok}"
            return self._json({"token": tok, "url": url, "expires_at": expires}, 201)

        # GET /api/org/invites → list invites created by this user (gateway-level)
        if path == "/api/org/invites" and self.command == "GET":
            rows = db.execute(
                "SELECT token, email, created_at, expires_at, used_at, used_by "
                "FROM org_invites WHERE owner_id=? AND used_at IS NULL AND expires_at > ? "
                "ORDER BY created_at DESC",
                (user_id, now)
            ).fetchall()
            base = self._base_url()
            return self._json([{**dict(r), "url": f"{base}/invite/{r['token']}"} for r in rows])

        # DELETE /api/org/invites/<token>
        if path.startswith("/api/org/invites/") and self.command == "DELETE":
            tok = path[len("/api/org/invites/"):]
            db.execute("DELETE FROM org_invites WHERE token=? AND owner_id=?", (tok, user_id))
            db.commit()
            return self._json({"ok": True})

        # GET /api/gateway/orgs → list orgs accessible to this user
        if path == "/api/gateway/orgs" and self.command == "GET":
            orgs = [{"id": user_id, "email": user_email, "is_own": True}]
            member_rows = db.execute(
                "SELECT u.id, u.email FROM org_members m JOIN users u ON m.owner_id = u.id "
                "WHERE m.member_id=?", (user_id,)
            ).fetchall()
            for r in member_rows:
                orgs.append({"id": r["id"], "email": r["email"], "is_own": False})
            return self._json(orgs)

        # POST /api/gateway/switch-org → set amux_org cookie
        if path == "/api/gateway/switch-org" and self.command == "POST":
            body = self._read_body()
            org_id = body.get("org_id", "").strip()
            sec = self._secure_cookie_flags()
            if org_id == user_id or not org_id:
                # Switch back to own workspace
                return self._redirect(
                    self._base_url() + "/",
                    extra_cookies=[f"amux_org=; Max-Age=0; Path=/; HttpOnly{sec}; SameSite=Lax"]
                )
            member_row = db.execute(
                "SELECT 1 FROM org_members WHERE owner_id=? AND member_id=?",
                (org_id, user_id)
            ).fetchone()
            if not member_row:
                return self._json({"error": "not a member of this workspace"}, 403)
            return self._redirect(
                self._base_url() + "/",
                extra_cookies=[f"amux_org={org_id}; HttpOnly{sec}; SameSite=Lax; Path=/"]
            )

        # GET /api/gateway/members → list members of your workspace
        if path == "/api/gateway/members" and self.command == "GET":
            rows = db.execute(
                "SELECT COALESCE(NULLIF(u.email,''), m.member_email) AS email, m.member_id, m.joined_at "
                "FROM org_members m JOIN users u ON m.member_id = u.id "
                "WHERE m.owner_id=? ORDER BY m.joined_at",
                (user_id,)
            ).fetchall()
            return self._json([dict(r) for r in rows])

        # DELETE /api/gateway/members/<member_id> → remove from workspace
        if path.startswith("/api/gateway/members/") and self.command == "DELETE":
            mid = path[len("/api/gateway/members/"):]
            db.execute("DELETE FROM org_members WHERE owner_id=? AND member_id=?", (user_id, mid))
            db.commit()
            return self._json({"ok": True})

        # ── Stripe billing (authenticated) ────────────────────────────────────
        if path == "/api/stripe/checkout" and self.command == "POST":
            if not STRIPE_SECRET_KEY or not STRIPE_PRO_PRICE_ID:
                return self._json({"error": "billing not configured"}, 503)
            import stripe
            stripe.api_key = STRIPE_SECRET_KEY
            base = self._base_url()
            session = stripe.checkout.Session.create(
                mode="subscription",
                line_items=[{"price": STRIPE_PRO_PRICE_ID, "quantity": 1}],
                client_reference_id=user_id,
                customer_email=user_email,
                success_url=base + "/?billing=success",
                cancel_url=base + "/?billing=cancel",
                allow_promotion_codes=True,
            )
            return self._json({"url": session.url})

        if path == "/api/stripe/portal" and self.command == "POST":
            if not STRIPE_SECRET_KEY:
                return self._json({"error": "billing not configured"}, 503)
            row_u = db.execute("SELECT stripe_customer_id FROM users WHERE id=?", (user_id,)).fetchone()
            cust_id = row_u["stripe_customer_id"] if row_u else None
            if not cust_id:
                return self._json({"error": "no billing account"}, 404)
            import stripe
            stripe.api_key = STRIPE_SECRET_KEY
            base = self._base_url()
            ps = stripe.billing_portal.Session.create(
                customer=cust_id,
                return_url=base + "/",
            )
            return self._json({"url": ps.url})

        if path == "/api/stripe/status" and self.command == "GET":
            row_u = db.execute("SELECT plan, stripe_customer_id FROM users WHERE id=?", (user_id,)).fetchone()
            return self._json({
                "plan": row_u["plan"] if row_u else "free",
                "has_billing": bool(row_u and row_u["stripe_customer_id"]),
                "stripe_configured": bool(STRIPE_SECRET_KEY),
            })

        # ── Determine target container ─────────────────────────────────────────
        # Org members always share the owner's container — no separate workspace.
        target_user_id = user_id
        target_port = port
        target_email = user_email

        # Check if this user is a member of someone else's org
        org_row = db.execute(
            "SELECT m.owner_id, u.port, u.email FROM org_members m "
            "JOIN users u ON m.owner_id = u.id WHERE m.member_id=?",
            (user_id,)
        ).fetchone()
        if org_row and org_row["port"]:
            target_user_id = org_row["owner_id"]
            target_port = org_row["port"]
            # Keep the member's own email for X-Amux-User-Email header
            # Also refresh the owner's last_seen so the reaper doesn't kill
            # the shared container while an org member is actively using it.
            now = int(time.time())
            db.execute("UPDATE users SET last_seen=? WHERE id=?", (now, target_user_id))
            db.commit()

        # Wake target container if needed
        if not container_healthy(target_user_id):
            try:
                start_container(target_user_id, target_port)
            except Exception as e:
                return self._json({"error": f"failed to start instance: {e}"}, 503)

        proxy(self, target_port, path, qs, user_email=target_email, user_id=target_user_id)

    def do_GET(self):    self._handle()
    def do_POST(self):   self._handle()
    def do_PATCH(self):  self._handle()
    def do_DELETE(self): self._handle()
    def do_PUT(self):    self._handle()

# ── Main ───────────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    os.makedirs(DATA_DIR, exist_ok=True)
    get_db()
    print(f"[gateway] listening on :{PORT}")
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
