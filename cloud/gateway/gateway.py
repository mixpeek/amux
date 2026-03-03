#!/usr/bin/env python3
"""
amux cloud gateway — auth + per-user container orchestration
Verifies Clerk JWTs, starts/stops Docker containers per user, reverse-proxies requests.
"""

import os, json, time, sqlite3, subprocess, threading, urllib.request, urllib.error, base64
import hmac, hashlib
from http.server import HTTPServer, BaseHTTPRequestHandler

# ── Config ────────────────────────────────────────────────────────────────────
CLERK_PUBLISHABLE_KEY = os.environ["CLERK_PUBLISHABLE_KEY"]
CLERK_SECRET_KEY      = os.environ["CLERK_SECRET_KEY"]
R2_ACCESS_KEY         = os.environ["R2_ACCESS_KEY"]
R2_SECRET_KEY         = os.environ["R2_SECRET_KEY"]
CF_ACCOUNT_ID         = os.environ["CF_ACCOUNT_ID"]
COOKIE_SECRET         = os.environ.get("COOKIE_SECRET", "change-me")

PORT          = int(os.environ.get("GATEWAY_PORT", "8080"))
COMPOSE_TPL   = os.path.join(os.path.dirname(__file__), "../docker/docker-compose.template.yml")
LITESTREAM_YML= os.path.join(os.path.dirname(__file__), "../litestream/litestream.yml")
DATA_DIR      = os.environ.get("AMUX_CLOUD_DATA", "/var/amux/users")
DB_PATH       = os.environ.get("GATEWAY_DB", "/var/amux/gateway.db")
IDLE_SECONDS  = int(os.environ.get("IDLE_TIMEOUT", "600"))
PORT_BASE     = 9000
COOKIE_MAX_AGE = 86400 * 7  # 7 days

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
  <div id="status"></div>
  <script>
    const PK = '__CLERK_PK__';
    let exchanging = false;

    function setStatus(msg) {
      document.getElementById('status').textContent = msg;
    }

    async function exchangeAndRedirect() {
      if (exchanging) return;
      exchanging = true;
      document.getElementById('clerk-root').innerHTML = '<div class="spinner"></div>';
      setStatus('Starting your workspace\u2026');
      try {
        const token = await window.Clerk.session.getToken();
        const res = await fetch('/api/cloud-auth', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ token })
        });
        if (res.ok) {
          window.location.replace('/');
        } else {
          const d = await res.json().catch(() => ({}));
          document.getElementById('clerk-root').innerHTML = '';
          setStatus('Auth error: ' + (d.error || res.status));
          exchanging = false;
        }
      } catch (e) {
        document.getElementById('clerk-root').innerHTML = '';
        setStatus('Connection error — please refresh.');
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
            last_seen   INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS waitlist (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL UNIQUE,
            ts    INTEGER NOT NULL
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

def start_container(user_id, port):
    _write_compose(user_id, port)
    d = _compose_dir(user_id)
    subprocess.run(["docker", "compose", "up", "-d"], cwd=d,
                   capture_output=True, check=True)
    for _ in range(20):
        time.sleep(1)
        if container_running(user_id):
            break

def stop_container(user_id):
    d = _compose_dir(user_id)
    subprocess.run(["docker", "compose", "stop"], cwd=d, capture_output=True)

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

# ── Idle reaper ────────────────────────────────────────────────────────────────
def _reaper():
    while True:
        time.sleep(60)
        try:
            db = get_db()
            cutoff = int(time.time()) - IDLE_SECONDS
            stale = db.execute(
                "SELECT id FROM users WHERE last_seen < ? AND plan = 'free'",
                (cutoff,)).fetchall()
            for row in stale:
                uid = row["id"]
                if container_running(uid):
                    print(f"[reaper] stopping idle container for {uid}")
                    stop_container(uid)
        except Exception as e:
            print(f"[reaper] error: {e}")

threading.Thread(target=_reaper, daemon=True).start()

# ── Proxy helper ───────────────────────────────────────────────────────────────
def proxy(handler, port, path, qs):
    url = f"http://127.0.0.1:{port}{path}"
    if qs:
        url += "?" + qs
    length = int(handler.headers.get("Content-Length", 0))
    body = handler.rfile.read(length) if length else None
    # Strip auth headers so container doesn't see them
    skip = {"host", "content-length", "authorization", "cookie"}
    req = urllib.request.Request(url, data=body, method=handler.command,
                                  headers={k: v for k, v in handler.headers.items()
                                           if k.lower() not in skip})
    try:
        resp = urllib.request.urlopen(req, timeout=60)
        handler.send_response(resp.status)
        for k, v in resp.headers.items():
            if k.lower() not in ("transfer-encoding",):
                handler.send_header(k, v)
        handler.end_headers()
        handler.wfile.write(resp.read())
    except urllib.error.HTTPError as e:
        handler.send_response(e.code)
        handler.end_headers()
        handler.wfile.write(e.read())

# ── Request handler ────────────────────────────────────────────────────────────
class Handler(BaseHTTPRequestHandler):
    log_message = lambda *a: None

    def _json(self, d, code=200):
        body = json.dumps(d).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body)

    def _serve_login(self, invite_token=""):
        html = _LOGIN_HTML.replace("__CLERK_PK__", CLERK_PUBLISHABLE_KEY)
        if invite_token:
            redirect_path = f"/?invite_token={invite_token}"
            html = html.replace("window.location.replace('/')",
                                f"window.location.replace('{redirect_path}')")
        body = html.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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

        # ── Public: exchange Clerk JWT for session cookie ──
        if path == "/api/cloud-auth" and self.command == "POST":
            length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(length)) if length else {}
            token = body.get("token", "")
            try:
                user_id, email = verify_clerk_token(token)
            except Exception as e:
                return self._json({"error": f"invalid token: {e}"}, 401)
            # Upsert user
            db = get_db()
            now = int(time.time())
            with _db_lock:
                row = db.execute("SELECT id FROM users WHERE id=?", (user_id,)).fetchone()
                if not row:
                    port = alloc_port(db)
                    db.execute(
                        "INSERT INTO users (id, email, plan, port, created_at, last_seen) VALUES (?,?,?,?,?,?)",
                        (user_id, email, "free", port, now, now))
                    db.commit()
                else:
                    db.execute("UPDATE users SET last_seen=?, email=? WHERE id=?",
                               (now, email, user_id))
                    db.commit()
            cookie_val = _make_cookie(user_id)
            resp_body = json.dumps({"ok": True}).encode()
            is_https = self.headers.get("X-Forwarded-Proto", "") == "https"
            secure_flag = "; Secure" if is_https else ""
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(resp_body)))
            self.send_header("Set-Cookie",
                f"amux_session={cookie_val}; HttpOnly{secure_flag}; SameSite=Lax; "
                f"Max-Age={COOKIE_MAX_AGE}; Path=/")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(resp_body)
            return

        # ── Unauthenticated invite links — serve login with redirect ──────────
        if path.startswith("/invite/") and self.command == "GET":
            cookies = _parse_cookies(self.headers.get("Cookie", ""))
            if not cookies.get("amux_session"):
                accept = self.headers.get("Accept", "")
                if "text/html" in accept:
                    invite_token = path[len("/invite/"):]
                    return self._serve_login(invite_token=invite_token)

        # ── Resolve user: Bearer token OR session cookie ──
        user_id = None
        email   = ""
        auth = self.headers.get("Authorization", "")
        if auth.startswith("Bearer "):
            try:
                user_id, email = verify_clerk_token(auth[7:])
            except Exception as e:
                return self._json({"error": f"invalid token: {e}"}, 401)
        else:
            cookies = _parse_cookies(self.headers.get("Cookie", ""))
            session_val = cookies.get("amux_session", "")
            if session_val:
                try:
                    user_id = _verify_cookie(session_val)
                except ValueError:
                    # Expired/invalid — send back to login
                    accept = self.headers.get("Accept", "")
                    if "text/html" in accept:
                        return self._serve_login()
                    return self._json({"error": "session expired"}, 401)
            else:
                # No auth — serve login page for browsers, 401 for API
                accept = self.headers.get("Accept", "")
                if "text/html" in accept:
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

        # Wake container if needed
        if not container_running(user_id):
            try:
                start_container(user_id, port)
            except Exception as e:
                return self._json({"error": f"failed to start instance: {e}"}, 503)

        proxy(self, port, path, qs)

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
    HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
