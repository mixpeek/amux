#!/usr/bin/env python3
"""
amux cloud gateway — auth + per-user container orchestration
Verifies Clerk JWTs, starts/stops Docker containers per user, reverse-proxies requests.
"""

import os, json, time, sqlite3, subprocess, threading, urllib.request, urllib.error, base64
from http.server import HTTPServer, BaseHTTPRequestHandler

# ── Config ────────────────────────────────────────────────────────────────────
CLERK_PUBLISHABLE_KEY = os.environ["CLERK_PUBLISHABLE_KEY"]   # pk_test_...
CLERK_SECRET_KEY      = os.environ["CLERK_SECRET_KEY"]        # sk_test_...
R2_ACCESS_KEY         = os.environ["R2_ACCESS_KEY"]
R2_SECRET_KEY         = os.environ["R2_SECRET_KEY"]
CF_ACCOUNT_ID         = os.environ["CF_ACCOUNT_ID"]

PORT          = int(os.environ.get("GATEWAY_PORT", "8080"))
COMPOSE_TPL   = os.path.join(os.path.dirname(__file__), "../docker/docker-compose.template.yml")
LITESTREAM_YML= os.path.join(os.path.dirname(__file__), "../litestream/litestream.yml")
DATA_DIR      = os.environ.get("AMUX_CLOUD_DATA", "/var/amux/users")
DB_PATH       = os.environ.get("GATEWAY_DB", "/var/amux/gateway.db")
IDLE_SECONDS  = int(os.environ.get("IDLE_TIMEOUT", "600"))   # stop after 10 min idle
PORT_BASE     = 9000   # user containers get ports 9000, 9001, ...

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
    # Wait up to 20s for healthy
    for _ in range(20):
        time.sleep(1)
        if container_running(user_id):
            break

def stop_container(user_id):
    d = _compose_dir(user_id)
    subprocess.run(["docker", "compose", "stop"], cwd=d, capture_output=True)

# ── Clerk JWT verification ─────────────────────────────────────────────────────
_jwks_cache = {"keys": None, "ts": 0}
_jwks_lock  = threading.Lock()

def _get_jwks():
    with _jwks_lock:
        if _jwks_cache["keys"] and time.time() - _jwks_cache["ts"] < 3600:
            return _jwks_cache["keys"]
    # Derive JWKS URL from publishable key
    # pk_test_cmVzb2x2ZWQtY3Jvdy00OS5jbGVyay5hY2NvdW50cy5kZXYk → base64 decode → domain
    raw = CLERK_PUBLISHABLE_KEY.split("_", 2)[2]
    # pad base64
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
    """Verify a Clerk session JWT. Returns user_id (sub) or raises."""
    import jwt as pyjwt  # pip install PyJWT[crypto]
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
    req = urllib.request.Request(url, data=body, method=handler.command,
                                  headers={k: v for k, v in handler.headers.items()
                                           if k.lower() not in ("host", "content-length")})
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
    log_message = lambda *a: None  # suppress default logging

    def _json(self, d, code=200):
        body = json.dumps(d).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
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
        from urllib.parse import urlparse, parse_qs
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

        # ── Auth required for all other routes ──
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("Bearer "):
            return self._json({"error": "unauthorized"}, 401)
        token = auth[7:]
        try:
            user_id, email = verify_clerk_token(token)
        except Exception as e:
            return self._json({"error": f"invalid token: {e}"}, 401)

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
        plan = row["plan"]

        # Wake container if needed
        if not container_running(user_id):
            try:
                start_container(user_id, port)
            except Exception as e:
                return self._json({"error": f"failed to start instance: {e}"}, 503)

        # Proxy to user's container
        proxy(self, port, path, qs)

    def do_GET(self):    self._handle()
    def do_POST(self):   self._handle()
    def do_PATCH(self):  self._handle()
    def do_DELETE(self): self._handle()
    def do_PUT(self):    self._handle()

# ── Main ───────────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    os.makedirs(DATA_DIR, exist_ok=True)
    get_db()  # init tables
    print(f"[gateway] listening on :{PORT}")
    HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
