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
STRIPE_SECRET_KEY       = os.environ.get("STRIPE_SECRET_KEY", "")
STRIPE_WEBHOOK_SECRET   = os.environ.get("STRIPE_WEBHOOK_SECRET", "")
STRIPE_PRO_PRICE_ID     = os.environ.get("STRIPE_PRO_PRICE_ID", "")      # monthly
STRIPE_ANNUAL_PRICE_ID  = os.environ.get("STRIPE_ANNUAL_PRICE_ID", "")   # annual
TRIAL_DAYS              = int(os.environ.get("TRIAL_DAYS", "7"))

PORT          = int(os.environ.get("GATEWAY_PORT", "8080"))
COMPOSE_TPL   = os.path.join(os.path.dirname(__file__), "../docker/docker-compose.template.yml")
LITESTREAM_YML= os.path.join(os.path.dirname(__file__), "../litestream/litestream.yml")
DATA_DIR      = os.environ.get("AMUX_CLOUD_DATA", "/var/amux/users")
DB_PATH       = os.environ.get("GATEWAY_DB", "/var/amux/gateway.db")
IDLE_SECONDS  = int(os.environ.get("IDLE_TIMEOUT", "3600"))
PORT_BASE     = 9000
COOKIE_MAX_AGE = 86400 * 7  # 7 days
# Signup is open — no passcode required
ADMIN_EMAILS    = set(e.strip() for e in os.environ.get("ADMIN_EMAILS", "").split(",") if e.strip())
GATEWAY_LOG     = "/var/log/amux-gateway.log"

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
  <div id="passcode-root" style="display:none;"></div>
  <div id="status"></div>
  <a href="https://apps.apple.com/us/app/amux-agent-multiplexer/id6760410435" target="_blank" rel="noopener" style="display:inline-flex;align-items:center;gap:6px;color:#888;font-size:0.82rem;text-decoration:none;margin-top:8px;">
    <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor"><path d="M18.71 19.5c-.83 1.24-1.71 2.45-3.05 2.47-1.34.03-1.77-.79-3.29-.79-1.53 0-2 .77-3.27.82-1.31.05-2.3-1.32-3.14-2.53C4.25 17 2.94 12.45 4.7 9.39c.87-1.52 2.43-2.48 4.12-2.51 1.28-.02 2.5.87 3.29.87.78 0 2.26-1.07 3.8-.91.65.03 2.47.26 3.64 1.98-.09.06-2.17 1.28-2.15 3.81.03 3.02 2.65 4.03 2.68 4.04-.03.07-.42 1.44-1.38 2.83M13 3.5c.73-.83 1.94-1.46 2.94-1.5.13 1.17-.34 2.35-1.04 3.19-.69.85-1.83 1.51-2.95 1.42-.15-1.15.41-2.35 1.05-3.11z"/></svg>
    Get the iOS App
  </a>
  <div id="self-host-back" style="display:none;margin-top:24px;">
    <a href="#" id="self-host-link" style="color:#888;font-size:0.82rem;text-decoration:underline;text-decoration-color:#444;text-underline-offset:3px;">Back to self-hosted</a>
  </div>
  <script>
    // Show "back to self-hosted" if user has amux_connections in localStorage
    try {
      const conns = JSON.parse(localStorage.getItem('amux_connections') || '[]');
      const selfHosted = conns.find(c => c.url && !c.url.includes('cloud.amux.io'));
      if (selfHosted) {
        const el = document.getElementById('self-host-back');
        const link = document.getElementById('self-host-link');
        el.style.display = '';
        link.href = selfHosted.url;
        link.textContent = 'Back to ' + (selfHosted.name || selfHosted.url);
      }
    } catch(e) {}
  </script>
  <script>
    const PK = '__CLERK_PK__';
    let exchanging = false;
    const POST_LOGIN_REDIRECT = '__POST_LOGIN_REDIRECT__';

    function setStatus(msg) {
      document.getElementById('status').textContent = msg;
    }

    async function exchangeAndRedirect() {
      if (exchanging) return;
      exchanging = true;
      const clerkEl = document.getElementById('clerk-root');
      clerkEl.innerHTML = '<div class="spinner"></div>';
      setStatus('Starting your workspace\u2026');
      try {
        const token = await window.Clerk.session.getToken();
        const email = window.Clerk.user?.primaryEmailAddress?.emailAddress || '';
        const res = await fetch('/api/cloud-auth', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ token, email })
        });
        if (res.ok) {
          window.location.replace(POST_LOGIN_REDIRECT || '/');
        } else {
          const d = await res.json().catch(() => ({}));
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
        // If redirected from logout, sign out of Clerk too
        if (new URLSearchParams(location.search).has('logout') && window.Clerk.user) {
          await window.Clerk.signOut();
        }
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

# ── Upgrade HTML (trial expired) ───────────────────────────────────────────────
_UPGRADE_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Upgrade — amux cloud</title>
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
      background: #0a0a0a; color: #e5e5e5;
      min-height: 100vh; display: flex; align-items: center; justify-content: center; }
    .wrap { max-width: 480px; width: 90%; text-align: center; }
    .logo { font-size: 1.4rem; font-weight: 700; color: #fff; margin-bottom: 32px; }
    .logo span { color: #555; font-weight: 400; }
    h1 { font-size: 1.3rem; margin-bottom: 8px; }
    p { color: #888; font-size: 0.88rem; margin-bottom: 28px; line-height: 1.5; }
    .plans { display: flex; flex-direction: column; gap: 12px; margin-bottom: 24px; }
    .plan { background: #1a1a1a; border: 1px solid #333; border-radius: 12px; padding: 20px;
      text-align: left; }
    .plan.featured { border-color: #7c6fcd; }
    .plan h3 { font-size: 1rem; margin-bottom: 4px; }
    .plan .price { color: #aaa; font-size: 0.82rem; margin-bottom: 12px; }
    .plan .save { color: #3fb950; font-size: 0.75rem; font-weight: 600; }
    .plan .features { color: #888; font-size: 0.78rem; margin-bottom: 14px; }
    .btn { display: inline-block; background: #7c6fcd; color: #fff; border: none;
      border-radius: 8px; padding: 10px 24px; font-size: 0.9rem; font-weight: 600;
      cursor: pointer; width: 100%; }
    .btn:hover { background: #9b8ee0; }
    .logout { color: #555; font-size: 0.78rem; margin-top: 16px; }
    .logout a { color: #888; text-decoration: underline; text-underline-offset: 3px; }
    #error { color: #f87171; font-size: 0.82rem; margin-top: 8px; min-height: 1.2em; }
  </style>
</head>
<body>
  <div class="wrap">
    <div class="logo">amux <span>cloud</span></div>
    <h1>Your free trial has ended</h1>
    <p>Subscribe to keep using your workspace. All your sessions and data are safe.</p>
    <div class="plans">
      <div class="plan">
        <h3>Pro Monthly</h3>
        <div class="price">$20/month</div>
        <div class="features">Unlimited sessions &middot; No idle timeout &middot; Team workspaces</div>
        <button class="btn" onclick="checkout('monthly')">Subscribe monthly</button>
      </div>
      <div class="plan featured">
        <h3>Pro Annual <span class="save">save 17%</span></h3>
        <div class="price">$200/year ($16.67/mo)</div>
        <div class="features">Unlimited sessions &middot; No idle timeout &middot; Team workspaces</div>
        <button class="btn" onclick="checkout('annual')">Subscribe annually</button>
      </div>
    </div>
    <div id="error"></div>
    <div class="logout"><a href="/api/cloud-logout">Log out</a></div>
  </div>
  <script>
    async function checkout(billing) {
      document.getElementById('error').textContent = '';
      try {
        const r = await fetch('/api/stripe/checkout', {
          method: 'POST', headers: {'Content-Type':'application/json'},
          body: JSON.stringify({ billing })
        });
        const d = await r.json();
        if (d.url) location.href = d.url;
        else document.getElementById('error').textContent = d.error || 'Failed to start checkout';
      } catch(e) { document.getElementById('error').textContent = 'Connection error'; }
    }
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
    try:
        conn.execute("SELECT trial_ends_at FROM users LIMIT 1")
    except sqlite3.OperationalError:
        conn.execute("ALTER TABLE users ADD COLUMN trial_ends_at INTEGER")
        conn.commit()
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS waitlist (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL UNIQUE,
            ts    INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS orgs (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            slug            TEXT UNIQUE,
            owner_id        TEXT NOT NULL,
            port            INTEGER UNIQUE,
            plan            TEXT NOT NULL DEFAULT 'free',
            stripe_customer_id TEXT,
            stripe_subscription_id TEXT,
            trial_ends_at   INTEGER,
            api_key         TEXT,
            created_at      INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS org_memberships (
            org_id      TEXT NOT NULL,
            user_id     TEXT NOT NULL,
            role        TEXT NOT NULL DEFAULT 'member',
            joined_at   INTEGER NOT NULL,
            PRIMARY KEY (org_id, user_id)
        );
        CREATE TABLE IF NOT EXISTS org_invites (
            token       TEXT PRIMARY KEY,
            org_id      TEXT NOT NULL,
            email       TEXT,
            role        TEXT NOT NULL DEFAULT 'member',
            created_at  INTEGER NOT NULL,
            expires_at  INTEGER NOT NULL,
            used_at     INTEGER,
            used_by     TEXT
        );
    """)
    # ── Migrate: user-as-org → proper orgs table ──
    # If users still have port column and orgs table is empty, migrate
    try:
        has_port = conn.execute("SELECT port FROM users LIMIT 1").fetchone()
    except sqlite3.OperationalError:
        has_port = None
    if has_port is not None:
        org_count = conn.execute("SELECT COUNT(*) FROM orgs").fetchone()[0]
        if org_count == 0:
            # Migrate each user to a personal org (org.id = user.id)
            rows = conn.execute("SELECT id, email, plan, port, created_at, stripe_customer_id, stripe_subscription_id, trial_ends_at FROM users WHERE port IS NOT NULL").fetchall()
            for r in rows:
                conn.execute(
                    "INSERT OR IGNORE INTO orgs (id, name, slug, owner_id, port, plan, stripe_customer_id, stripe_subscription_id, trial_ends_at, created_at) "
                    "VALUES (?,?,?,?,?,?,?,?,?,?)",
                    (r["id"], r["email"] or r["id"], None, r["id"], r["port"], r["plan"],
                     r["stripe_customer_id"], r["stripe_subscription_id"],
                     r["trial_ends_at"], r["created_at"]))
                conn.execute(
                    "INSERT OR IGNORE INTO org_memberships (org_id, user_id, role, joined_at) VALUES (?,?,?,?)",
                    (r["id"], r["id"], "owner", r["created_at"]))
            # Migrate old org_members → org_memberships
            try:
                old_members = conn.execute("SELECT owner_id, member_id, joined_at FROM org_members").fetchall()
                for m in old_members:
                    conn.execute(
                        "INSERT OR IGNORE INTO org_memberships (org_id, user_id, role, joined_at) VALUES (?,?,?,?)",
                        (m["owner_id"], m["member_id"], "member", m["joined_at"]))
            except sqlite3.OperationalError:
                pass  # org_members table doesn't exist
            # Migrate old org_invites: owner_id → org_id
            try:
                old_invites = conn.execute("SELECT token, owner_id, email, created_at, expires_at, used_at, used_by FROM org_invites WHERE 1").fetchall()
                # Re-insert with org_id field (already created with new schema, but may have old data)
                for inv in old_invites:
                    try:
                        conn.execute("UPDATE org_invites SET org_id=? WHERE token=?", (inv["owner_id"], inv["token"]))
                    except sqlite3.OperationalError:
                        pass
            except (sqlite3.OperationalError, KeyError):
                pass
            conn.commit()
            print(f"[db] migrated {len(rows)} users to orgs table", flush=True)
    # Migrate: add api_key column if missing
    try:
        conn.execute("SELECT api_key FROM orgs LIMIT 1")
    except sqlite3.OperationalError:
        conn.execute("ALTER TABLE orgs ADD COLUMN api_key TEXT")
        conn.commit()
    # Migrate: add org_id + role columns to org_invites if missing (old schema had owner_id)
    try:
        conn.execute("SELECT org_id FROM org_invites LIMIT 1")
    except sqlite3.OperationalError:
        try:
            conn.execute("ALTER TABLE org_invites ADD COLUMN org_id TEXT NOT NULL DEFAULT ''")
        except sqlite3.OperationalError:
            pass
        try:
            conn.execute("ALTER TABLE org_invites ADD COLUMN role TEXT NOT NULL DEFAULT 'member'")
        except sqlite3.OperationalError:
            pass
        # Backfill org_id from owner_id
        try:
            conn.execute("UPDATE org_invites SET org_id = owner_id WHERE org_id = ''")
        except sqlite3.OperationalError:
            pass
        conn.commit()
    # Backfill trial_ends_at for existing free orgs that don't have one
    conn.execute(
        "UPDATE orgs SET trial_ends_at = created_at + ? WHERE plan = 'free' AND trial_ends_at IS NULL",
        (TRIAL_DAYS * 86400,))
    conn.execute(
        "UPDATE users SET trial_ends_at = created_at + ? WHERE plan = 'free' AND trial_ends_at IS NULL",
        (TRIAL_DAYS * 86400,))
    conn.commit()
    return conn

# ── Port allocation ────────────────────────────────────────────────────────────
def alloc_port(db):
    used = {r[0] for r in db.execute("SELECT port FROM orgs WHERE port IS NOT NULL")}
    # Also check legacy users table for transition period
    try:
        used |= {r[0] for r in db.execute("SELECT port FROM users WHERE port IS NOT NULL")}
    except sqlite3.OperationalError:
        pass
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

def _push_org_api_key(org_id, api_key):
    """Write the org's shared API key into the container's server.env."""
    ctr = f"amux-user-{org_id}"
    try:
        # Read existing server.env from container
        r = subprocess.run(
            ["docker", "exec", ctr, "cat", "/root/.amux/server.env"],
            capture_output=True, text=True)
        lines = r.stdout.splitlines() if r.returncode == 0 else []
        # Update or add ANTHROPIC_API_KEY
        found = False
        for i, line in enumerate(lines):
            if line.startswith("ANTHROPIC_API_KEY="):
                lines[i] = f"ANTHROPIC_API_KEY={api_key}" if api_key else ""
                found = True
                break
        if not found and api_key:
            lines.append(f"ANTHROPIC_API_KEY={api_key}")
        content = "\n".join(l for l in lines if l.strip()) + "\n"
        # Write back
        subprocess.run(
            ["docker", "exec", "-i", ctr, "sh", "-c", "cat > /root/.amux/server.env"],
            input=content.encode(), capture_output=True)
        # Touch amux-server.py to trigger reload
        subprocess.run(
            ["docker", "exec", ctr, "touch", "/app/amux-server.py"],
            capture_output=True)
        print(f"[org] pushed API key to {org_id}", flush=True)
    except Exception as e:
        print(f"[org] failed to push API key to {org_id}: {e}", flush=True)

def start_container(user_id, port):
    _write_compose(user_id, port)
    _restore_user_files(user_id)
    # Inject org API key into server.env before starting
    try:
        db = get_db()
        org_row = db.execute("SELECT api_key FROM orgs WHERE id=?", (user_id,)).fetchone()
        if org_row and org_row["api_key"]:
            vol = f"amux-data-{user_id}"
            # Write server.env into the volume via a temp container
            env_content = f"ANTHROPIC_API_KEY={org_row['api_key']}\n"
            subprocess.run(
                ["docker", "run", "--rm", "-i", "-v", f"{vol}:/root/.amux",
                 "alpine:latest", "sh", "-c", """
                    # Merge org key into server.env without overwriting user keys
                    ENV=/root/.amux/server.env
                    if [ -f "$ENV" ] && grep -q "^ANTHROPIC_API_KEY=" "$ENV"; then
                        true  # user already has a key, don't override
                    else
                        echo "ANTHROPIC_API_KEY=$1" >> "$ENV"
                    fi
                 """, "--", org_row["api_key"]],
                capture_output=True)
    except Exception as e:
        print(f"[org] failed to inject API key for {user_id}: {e}", flush=True)
    d = _compose_dir(user_id)
    r = subprocess.run(["docker", "compose", "up", "-d"], cwd=d,
                       capture_output=True, text=True)
    if r.returncode != 0:
        err = (r.stderr or r.stdout or "unknown error").strip()
        print(f"[docker] compose up failed for {user_id}: {err}", flush=True)
        raise subprocess.CalledProcessError(r.returncode, r.args, r.stdout, r.stderr)
    # Wait for healthy (amux-server.py ready), not just running
    for _ in range(40):
        time.sleep(1)
        if container_healthy(user_id):
            break

def stop_container(user_id):
    d = _compose_dir(user_id)
    if os.path.isdir(d):
        subprocess.run(["docker", "compose", "down"], cwd=d, capture_output=True)

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
    rows = db.execute("SELECT id, port FROM orgs WHERE port IS NOT NULL").fetchall()
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
                ref_id = obj.get("client_reference_id")  # org_id (or legacy user_id)
                sub_id = obj.get("subscription")
                if ref_id and cust_id:
                    trial_end = None
                    if sub_id:
                        try:
                            import stripe as _s
                            _s.api_key = STRIPE_SECRET_KEY
                            sub_obj = _s.Subscription.retrieve(sub_id)
                            if sub_obj.trial_end:
                                trial_end = sub_obj.trial_end
                        except Exception:
                            pass
                    with _db_lock:
                        db.execute(
                            "UPDATE orgs SET plan='pro', stripe_customer_id=?, stripe_subscription_id=?, trial_ends_at=? WHERE id=?",
                            (cust_id, sub_id, trial_end, ref_id))
                        # Also update legacy users table for transition
                        db.execute(
                            "UPDATE users SET plan='pro', stripe_customer_id=?, stripe_subscription_id=?, trial_ends_at=? WHERE id=?",
                            (cust_id, sub_id, trial_end, ref_id))
                        db.commit()
                    print(f"[stripe] activated pro for org {ref_id} cust={cust_id} trial_end={trial_end}", flush=True)
            elif etype == "invoice.paid":
                cust_id = obj.get("customer")
                if cust_id:
                    with _db_lock:
                        db.execute("UPDATE orgs SET plan='pro' WHERE stripe_customer_id=?", (cust_id,))
                        db.execute("UPDATE users SET plan='pro' WHERE stripe_customer_id=?", (cust_id,))
                        db.commit()
            elif etype in ("customer.subscription.deleted", "customer.subscription.paused"):
                cust_id = obj.get("customer")
                if cust_id:
                    with _db_lock:
                        db.execute(
                            "UPDATE orgs SET plan='free', stripe_subscription_id=NULL WHERE stripe_customer_id=?",
                            (cust_id,))
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
            self.send_header("Location", "/?logout")
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
            trial_end = now + TRIAL_DAYS * 86400
            with _db_lock:
                row = db.execute("SELECT id FROM users WHERE id=?", (user_id,)).fetchone()
                if not row:
                    # New user — open signup with free trial
                    port = alloc_port(db)
                    db.execute(
                        "INSERT INTO users (id, email, plan, port, created_at, last_seen, trial_ends_at) VALUES (?,?,?,?,?,?,?)",
                        (user_id, email, "free", port, now, now, trial_end))
                    # Create personal org (id = user_id for Docker volume compat)
                    db.execute(
                        "INSERT OR IGNORE INTO orgs (id, name, slug, owner_id, port, plan, trial_ends_at, created_at) VALUES (?,?,?,?,?,?,?,?)",
                        (user_id, email or user_id, None, user_id, port, "free", trial_end, now))
                    db.execute(
                        "INSERT OR IGNORE INTO org_memberships (org_id, user_id, role, joined_at) VALUES (?,?,?,?)",
                        (user_id, user_id, "owner", now))
                    db.commit()
                    print(f"[signup] new user {email} ({user_id}) trial_ends={trial_end}", flush=True)
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
        trial_end_upsert = now + TRIAL_DAYS * 86400
        with _db_lock:
            row = db.execute("SELECT * FROM users WHERE id=?", (user_id,)).fetchone()
            if not row:
                port = alloc_port(db)
                db.execute(
                    "INSERT INTO users (id, email, plan, port, created_at, last_seen, trial_ends_at) VALUES (?,?,?,?,?,?,?)",
                    (user_id, email, "free", port, now, now, trial_end_upsert))
                db.execute(
                    "INSERT OR IGNORE INTO orgs (id, name, slug, owner_id, port, plan, trial_ends_at, created_at) VALUES (?,?,?,?,?,?,?,?)",
                    (user_id, email or user_id, None, user_id, port, "free", trial_end_upsert, now))
                db.execute(
                    "INSERT OR IGNORE INTO org_memberships (org_id, user_id, role, joined_at) VALUES (?,?,?,?)",
                    (user_id, user_id, "owner", now))
                db.commit()
                row = db.execute("SELECT * FROM users WHERE id=?", (user_id,)).fetchone()
            else:
                db.execute("UPDATE users SET last_seen=? WHERE id=?", (now, user_id))
                db.commit()
            # Ensure personal org exists (migration backfill)
            org_exists = db.execute("SELECT 1 FROM orgs WHERE id=?", (user_id,)).fetchone()
            if not org_exists and row["port"]:
                db.execute(
                    "INSERT OR IGNORE INTO orgs (id, name, slug, owner_id, port, plan, created_at) VALUES (?,?,?,?,?,?,?)",
                    (user_id, row["email"] or user_id, None, user_id, row["port"], row["plan"], row["created_at"]))
                db.execute(
                    "INSERT OR IGNORE INTO org_memberships (org_id, user_id, role, joined_at) VALUES (?,?,?,?)",
                    (user_id, user_id, "owner", row["created_at"]))
                db.commit()

        user_email = row["email"] or email
        # If we still don't have an email, fetch from Clerk API and persist it
        if not user_email:
            user_email = _clerk_get_email(user_id)
            if user_email:
                with _db_lock:
                    db.execute("UPDATE users SET email=? WHERE id=?", (user_email, user_id))
                    db.commit()

        # ── Gateway-level org/invite interceptors ─────────────────────────────

        # ── Helper: resolve org_id from cookie or default to personal ──
        def _active_org_id():
            cookies = _parse_cookies(self.headers.get("Cookie", ""))
            oid = cookies.get("amux_org", "")
            if oid:
                mem = db.execute("SELECT 1 FROM org_memberships WHERE org_id=? AND user_id=?", (oid, user_id)).fetchone()
                if mem:
                    return oid
            return user_id  # personal org

        # ── Helper: check if user has role in org ──
        def _has_role(org_id, *roles):
            r = db.execute("SELECT role FROM org_memberships WHERE org_id=? AND user_id=?", (org_id, user_id)).fetchone()
            return r and r["role"] in roles

        # GET /invite/<token> while authenticated → show accept page
        if path.startswith("/invite/") and self.command == "GET":
            tok = path[len("/invite/"):]
            inv = db.execute(
                "SELECT org_id FROM org_invites WHERE token=? AND used_at IS NULL AND expires_at > ?",
                (tok, now)
            ).fetchone()
            if not inv:
                return self._html("<html><body style='font-family:sans-serif;background:#0a0a0a;color:#e5e5e5;display:flex;align-items:center;justify-content:center;min-height:100vh;'><div style='text-align:center'><h2 style='color:#f87171'>Invite expired or invalid</h2><p style='color:#888;margin-top:8px'>This invite link is no longer valid.</p></div></body></html>", 410)
            org = db.execute("SELECT name, owner_id FROM orgs WHERE id=?", (inv["org_id"],)).fetchone()
            org_label = org["name"] if org else "a workspace"
            if org and org["owner_id"] == user_id:
                return self._html("<html><body style='font-family:sans-serif;background:#0a0a0a;color:#e5e5e5;display:flex;align-items:center;justify-content:center;min-height:100vh;'><div style='text-align:center'><h2>That's your own invite link!</h2><p style='color:#888;margin-top:8px'>Share it with someone else.</p></div></body></html>")
            return self._serve_invite_accept(tok, org_label)

        # POST /api/gateway/invite/<token>/accept → accept invite, set amux_org, redirect
        if path.startswith("/api/gateway/invite/") and path.endswith("/accept"):
            tok = path[len("/api/gateway/invite/"):-len("/accept")]
            inv = db.execute(
                "SELECT org_id, role FROM org_invites WHERE token=? AND used_at IS NULL AND expires_at > ?",
                (tok, now)
            ).fetchone()
            if not inv:
                return self._json({"error": "invalid or expired invite"}, 410)
            org_id = inv["org_id"]
            role = inv["role"] or "member"
            db.execute("UPDATE org_invites SET used_at=?, used_by=? WHERE token=?",
                       (now, user_id, tok))
            db.execute(
                "INSERT OR IGNORE INTO org_memberships (org_id, user_id, role, joined_at) "
                "VALUES (?,?,?,?)", (org_id, user_id, role, now))
            db.commit()
            sec = self._secure_cookie_flags()
            return self._redirect(
                self._base_url() + "/",
                extra_cookies=[f"amux_org={org_id}; HttpOnly{sec}; SameSite=Lax; Path=/"]
            )

        # POST /api/org/invites → create invite for an org
        if path == "/api/org/invites" and self.command == "POST":
            import secrets as _sec
            body = self._read_body()
            org_id = body.get("org_id", "") or _active_org_id()
            if not _has_role(org_id, "owner", "admin"):
                return self._json({"error": "must be owner or admin"}, 403)
            tok = _sec.token_urlsafe(24)
            expires = now + 7 * 86400
            db.execute(
                "INSERT INTO org_invites (token, org_id, owner_id, email, role, created_at, expires_at) "
                "VALUES (?,?,?,?,?,?,?)",
                (tok, org_id, user_id, body.get("email") or None, body.get("role", "member"), now, expires)
            )
            db.commit()
            url = f"{self._base_url()}/invite/{tok}"
            return self._json({"token": tok, "url": url, "org_id": org_id, "expires_at": expires}, 201)

        # GET /api/org/invites → list invites for orgs the user owns/admins
        if path == "/api/org/invites" and self.command == "GET":
            from urllib.parse import parse_qs
            params = parse_qs(qs)
            filter_org = params.get("org_id", [None])[0]
            if filter_org:
                owned_orgs = [filter_org] if _has_role(filter_org, "owner", "admin") else []
            else:
                owned_orgs = [r["org_id"] for r in db.execute(
                    "SELECT org_id FROM org_memberships WHERE user_id=? AND role IN ('owner','admin')", (user_id,)
                ).fetchall()]
            if not owned_orgs:
                return self._json([])
            placeholders = ",".join("?" * len(owned_orgs))
            rows = db.execute(
                f"SELECT token, org_id, email, role, created_at, expires_at, used_at, used_by "
                f"FROM org_invites WHERE org_id IN ({placeholders}) AND used_at IS NULL AND expires_at > ? "
                f"ORDER BY created_at DESC",
                (*owned_orgs, now)
            ).fetchall()
            base = self._base_url()
            return self._json([{**dict(r), "url": f"{base}/invite/{r['token']}"} for r in rows])

        # DELETE /api/org/invites/<token>
        if path.startswith("/api/org/invites/") and self.command == "DELETE":
            tok = path[len("/api/org/invites/"):]
            # Only org owner/admin can delete
            inv = db.execute("SELECT org_id FROM org_invites WHERE token=?", (tok,)).fetchone()
            if inv and _has_role(inv["org_id"], "owner", "admin"):
                db.execute("DELETE FROM org_invites WHERE token=?", (tok,))
                db.commit()
            return self._json({"ok": True})

        # ── Org CRUD ─────────────────────────────────────────────────────────

        # POST /api/gateway/orgs → create a new named org
        if path == "/api/gateway/orgs" and self.command == "POST":
            import secrets as _sec
            body = self._read_body()
            org_name = body.get("name", "").strip()
            if not org_name:
                return self._json({"error": "name is required"}, 400)
            org_id = "org_" + _sec.token_hex(8)
            org_port = alloc_port(db)
            with _db_lock:
                db.execute(
                    "INSERT INTO orgs (id, name, slug, owner_id, port, plan, created_at) VALUES (?,?,?,?,?,?,?)",
                    (org_id, org_name, body.get("slug"), user_id, org_port, "free", now))
                db.execute(
                    "INSERT INTO org_memberships (org_id, user_id, role, joined_at) VALUES (?,?,?,?)",
                    (org_id, user_id, "owner", now))
                db.commit()
            return self._json({"id": org_id, "name": org_name, "port": org_port}, 201)

        # GET /api/gateway/orgs → list orgs accessible to this user
        if path == "/api/gateway/orgs" and self.command == "GET":
            rows = db.execute(
                "SELECT o.id, o.name, o.slug, o.owner_id, o.plan, m.role "
                "FROM org_memberships m JOIN orgs o ON m.org_id = o.id "
                "WHERE m.user_id=? ORDER BY o.created_at",
                (user_id,)
            ).fetchall()
            cookies = _parse_cookies(self.headers.get("Cookie", ""))
            active = cookies.get("amux_org", user_id)
            return self._json([{
                "id": r["id"], "name": r["name"], "slug": r["slug"],
                "owner_id": r["owner_id"], "plan": r["plan"], "role": r["role"],
                "is_personal": r["id"] == user_id,
                "active": r["id"] == active,
            } for r in rows])

        # GET /api/gateway/orgs/<org_id> → org details
        if path.startswith("/api/gateway/orgs/") and self.command == "GET" and path.count("/") == 4 and not path.endswith("/members"):
            org_id = path.split("/")[4]
            if not db.execute("SELECT 1 FROM org_memberships WHERE org_id=? AND user_id=?", (org_id, user_id)).fetchone():
                return self._json({"error": "not a member"}, 403)
            org = db.execute("SELECT * FROM orgs WHERE id=?", (org_id,)).fetchone()
            if not org:
                return self._json({"error": "not found"}, 404)
            members = db.execute(
                "SELECT m.user_id, m.role, m.joined_at, u.email "
                "FROM org_memberships m JOIN users u ON m.user_id = u.id "
                "WHERE m.org_id=? ORDER BY m.joined_at", (org_id,)
            ).fetchall()
            api_key = org["api_key"] or ""
            masked_key = ("*" * (len(api_key) - 4) + api_key[-4:]) if len(api_key) > 8 else ("set" if api_key else "")
            return self._json({
                "id": org["id"], "name": org["name"], "slug": org["slug"],
                "owner_id": org["owner_id"], "plan": org["plan"],
                "has_api_key": bool(api_key),
                "api_key_hint": masked_key,
                "members": [dict(m) for m in members],
            })

        # PATCH /api/gateway/orgs/<org_id> → update org
        if path.startswith("/api/gateway/orgs/") and self.command == "PATCH" and path.count("/") == 4:
            org_id = path.split("/")[4]
            if not _has_role(org_id, "owner", "admin"):
                return self._json({"error": "must be owner or admin"}, 403)
            body = self._read_body()
            updates = []
            params = []
            if "name" in body:
                updates.append("name=?")
                params.append(body["name"])
            if "slug" in body:
                updates.append("slug=?")
                params.append(body["slug"])
            if "api_key" in body:
                updates.append("api_key=?")
                params.append(body["api_key"])
            if updates:
                params.append(org_id)
                with _db_lock:
                    db.execute(f"UPDATE orgs SET {','.join(updates)} WHERE id=?", params)
                    db.commit()
            # If API key was updated, write it into the running container's server.env
            if "api_key" in body:
                _push_org_api_key(org_id, body["api_key"])
            return self._json({"ok": True})

        # DELETE /api/gateway/orgs/<org_id> → delete org (owner only, not personal)
        if path.startswith("/api/gateway/orgs/") and self.command == "DELETE" and path.count("/") == 4:
            org_id = path.split("/")[4]
            if org_id == user_id:
                return self._json({"error": "cannot delete personal workspace"}, 400)
            if not _has_role(org_id, "owner"):
                return self._json({"error": "must be owner"}, 403)
            org = db.execute("SELECT port FROM orgs WHERE id=?", (org_id,)).fetchone()
            if org:
                try:
                    d = _compose_dir(org_id)
                    if os.path.isdir(d):
                        subprocess.run(["docker", "compose", "down", "--remove-orphans", "-v"],
                                       cwd=d, capture_output=True, timeout=30)
                        import shutil
                        shutil.rmtree(d, ignore_errors=True)
                except Exception as e:
                    print(f"[docker] failed to tear down {org_id}: {e}", flush=True)
                with _db_lock:
                    db.execute("DELETE FROM org_memberships WHERE org_id=?", (org_id,))
                    db.execute("DELETE FROM org_invites WHERE org_id=?", (org_id,))
                    db.execute("DELETE FROM orgs WHERE id=?", (org_id,))
                    db.commit()
            return self._json({"ok": True})

        # GET /api/gateway/orgs/<org_id>/members → list members
        if path.startswith("/api/gateway/orgs/") and path.endswith("/members") and self.command == "GET":
            org_id = path.split("/")[4]
            if not db.execute("SELECT 1 FROM org_memberships WHERE org_id=? AND user_id=?", (org_id, user_id)).fetchone():
                return self._json({"error": "not a member"}, 403)
            rows = db.execute(
                "SELECT m.user_id, m.role, m.joined_at, u.email "
                "FROM org_memberships m JOIN users u ON m.user_id = u.id "
                "WHERE m.org_id=? ORDER BY m.joined_at", (org_id,)
            ).fetchall()
            return self._json([dict(r) for r in rows])

        # DELETE /api/gateway/orgs/<org_id>/members/<user_id> → remove member
        if path.startswith("/api/gateway/orgs/") and "/members/" in path and self.command == "DELETE":
            parts = path.split("/")
            org_id = parts[4]
            target_uid = parts[6]
            if not _has_role(org_id, "owner", "admin"):
                return self._json({"error": "must be owner or admin"}, 403)
            if target_uid == user_id:
                return self._json({"error": "cannot remove yourself"}, 400)
            with _db_lock:
                db.execute("DELETE FROM org_memberships WHERE org_id=? AND user_id=?", (org_id, target_uid))
                db.commit()
            return self._json({"ok": True})

        # PATCH /api/gateway/orgs/<org_id>/members/<user_id> → change role
        if path.startswith("/api/gateway/orgs/") and "/members/" in path and self.command == "PATCH":
            parts = path.split("/")
            org_id = parts[4]
            target_uid = parts[6]
            if not _has_role(org_id, "owner"):
                return self._json({"error": "must be owner"}, 403)
            body = self._read_body()
            new_role = body.get("role", "member")
            if new_role not in ("owner", "admin", "member"):
                return self._json({"error": "invalid role"}, 400)
            with _db_lock:
                db.execute("UPDATE org_memberships SET role=? WHERE org_id=? AND user_id=?", (new_role, org_id, target_uid))
                db.commit()
            return self._json({"ok": True})

        # POST /api/gateway/switch-org → set amux_org cookie
        if path == "/api/gateway/switch-org" and self.command == "POST":
            body = self._read_body()
            org_id = body.get("org_id", "").strip()
            sec = self._secure_cookie_flags()
            if org_id == user_id or not org_id:
                # Switch back to personal workspace
                return self._redirect(
                    self._base_url() + "/",
                    extra_cookies=[f"amux_org=; Max-Age=0; Path=/; HttpOnly{sec}; SameSite=Lax"]
                )
            member_row = db.execute(
                "SELECT 1 FROM org_memberships WHERE org_id=? AND user_id=?",
                (org_id, user_id)
            ).fetchone()
            if not member_row:
                return self._json({"error": "not a member of this workspace"}, 403)
            return self._redirect(
                self._base_url() + "/",
                extra_cookies=[f"amux_org={org_id}; HttpOnly{sec}; SameSite=Lax; Path=/"]
            )

        # GET /api/gateway/members → list members of active org (backward compat)
        if path == "/api/gateway/members" and self.command == "GET":
            active_org = _active_org_id()
            rows = db.execute(
                "SELECT m.user_id AS member_id, u.email, m.role, m.joined_at "
                "FROM org_memberships m JOIN users u ON m.user_id = u.id "
                "WHERE m.org_id=? AND m.user_id != ? ORDER BY m.joined_at",
                (active_org, active_org)  # exclude the org itself for personal orgs
            ).fetchall()
            return self._json([dict(r) for r in rows])

        # DELETE /api/gateway/members/<member_id> → remove from active org (backward compat)
        if path.startswith("/api/gateway/members/") and self.command == "DELETE":
            mid = path[len("/api/gateway/members/"):]
            active_org = _active_org_id()
            if not _has_role(active_org, "owner", "admin"):
                return self._json({"error": "must be owner or admin"}, 403)
            with _db_lock:
                db.execute("DELETE FROM org_memberships WHERE org_id=? AND user_id=?", (active_org, mid))
                db.commit()
            return self._json({"ok": True})

        # ── Stripe billing (authenticated, org-scoped) ─────────────────────────
        if path == "/api/stripe/checkout" and self.command == "POST":
            if not STRIPE_SECRET_KEY or not STRIPE_PRO_PRICE_ID:
                return self._json({"error": "billing not configured"}, 503)
            body = self._read_body()
            billing = body.get("billing", "monthly")  # "monthly" or "annual"
            target_org = body.get("org_id", "") or _active_org_id()
            if not _has_role(target_org, "owner", "admin"):
                return self._json({"error": "must be owner or admin to manage billing"}, 403)
            price_id = STRIPE_ANNUAL_PRICE_ID if billing == "annual" and STRIPE_ANNUAL_PRICE_ID else STRIPE_PRO_PRICE_ID
            import stripe
            stripe.api_key = STRIPE_SECRET_KEY
            base = self._base_url()
            org_row = db.execute("SELECT stripe_customer_id, trial_ends_at FROM orgs WHERE id=?", (target_org,)).fetchone()
            has_had_trial = org_row and (org_row["stripe_customer_id"] or org_row["trial_ends_at"])
            checkout_params = dict(
                mode="subscription",
                line_items=[{"price": price_id, "quantity": 1}],
                client_reference_id=target_org,  # org_id as reference
                success_url=base + "/?billing=success",
                cancel_url=base + "/?billing=cancel",
                allow_promotion_codes=True,
            )
            if org_row and org_row["stripe_customer_id"]:
                checkout_params["customer"] = org_row["stripe_customer_id"]
            else:
                checkout_params["customer_email"] = user_email
            if not has_had_trial and TRIAL_DAYS > 0:
                checkout_params["subscription_data"] = {
                    "trial_period_days": TRIAL_DAYS,
                }
            session = stripe.checkout.Session.create(**checkout_params)
            return self._json({"url": session.url})

        if path == "/api/stripe/portal" and self.command == "POST":
            if not STRIPE_SECRET_KEY:
                return self._json({"error": "billing not configured"}, 503)
            body = self._read_body()
            target_org = body.get("org_id", "") or _active_org_id()
            if not _has_role(target_org, "owner", "admin"):
                return self._json({"error": "must be owner or admin to manage billing"}, 403)
            org_row = db.execute("SELECT stripe_customer_id FROM orgs WHERE id=?", (target_org,)).fetchone()
            cust_id = org_row["stripe_customer_id"] if org_row else None
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
            target_org = _active_org_id()
            org_row = db.execute("SELECT plan, stripe_customer_id, trial_ends_at FROM orgs WHERE id=?", (target_org,)).fetchone()
            now_ts = int(time.time())
            trial_ends = org_row["trial_ends_at"] if org_row else None
            in_trial = bool(trial_ends and trial_ends > now_ts)
            return self._json({
                "plan": org_row["plan"] if org_row else "free",
                "has_billing": bool(org_row and org_row["stripe_customer_id"]),
                "stripe_configured": bool(STRIPE_SECRET_KEY),
                "trial_ends_at": trial_ends,
                "in_trial": in_trial,
                "trial_days": TRIAL_DAYS,
                "has_annual": bool(STRIPE_ANNUAL_PRICE_ID),
                "org_id": target_org,
            })

        # ── Admin: gateway logs ───────────────────────────────────────────────
        if path.startswith("/api/gateway/logs") and self.command == "GET":
            if not ADMIN_EMAILS or user_email not in ADMIN_EMAILS:
                return self._json({"error": "forbidden"}, 403)

            from urllib.parse import parse_qs
            params = parse_qs(qs)
            lines = int(params.get("lines", ["200"])[0])
            search = params.get("search", [""])[0].lower()
            source = params.get("source", ["gateway"])[0]  # gateway | container

            if source == "container":
                org_id = params.get("org_id", [""])[0] or _active_org_id()
                # Verify user is a member of this org
                with _db_lock:
                    if not db.execute("SELECT 1 FROM org_memberships WHERE org_id=? AND user_id=?", (org_id, user_id)).fetchone():
                        return self._json({"error": "not a member of this workspace"}, 403)
                try:
                    result = subprocess.run(
                        ["docker", "logs", "--tail", str(lines), f"amux-user-{org_id}"],
                        capture_output=True, text=True, timeout=10)
                    raw = (result.stdout + result.stderr).strip().split("\n")
                except Exception as e:
                    return self._json({"error": f"docker logs failed: {e}"}, 500)
            else:
                try:
                    with open(GATEWAY_LOG, "r") as f:
                        raw = f.readlines()
                    raw = [l.rstrip("\n") for l in raw[-max(lines, 1):]]
                except FileNotFoundError:
                    raw = []

            if search:
                raw = [l for l in raw if search in l.lower()]

            return self._json({"lines": raw[-lines:], "count": len(raw), "source": source})

        # ── Admin: list containers ────────────────────────────────────────────
        if path == "/api/gateway/admin/containers" and self.command == "GET":
            if not ADMIN_EMAILS or user_email not in ADMIN_EMAILS:
                return self._json({"error": "forbidden"}, 403)
            try:
                result = subprocess.run(
                    ["docker", "ps", "--filter", "name=amux-user-", "--format",
                     "{{.Names}}\t{{.Status}}\t{{.Ports}}"],
                    capture_output=True, text=True, timeout=10)
                containers = []
                for line in result.stdout.strip().split("\n"):
                    if not line.strip():
                        continue
                    parts = line.split("\t", 2)
                    containers.append({
                        "name": parts[0],
                        "status": parts[1] if len(parts) > 1 else "",
                        "ports": parts[2] if len(parts) > 2 else "",
                    })
                return self._json({"containers": containers, "count": len(containers)})
            except Exception as e:
                return self._json({"error": str(e)}, 500)

        # ── Admin: list users + orgs overview ─────────────────────────────────
        if path == "/api/gateway/admin/users" and self.command == "GET":
            if not ADMIN_EMAILS or user_email not in ADMIN_EMAILS:
                return self._json({"error": "forbidden"}, 403)
            rows = db.execute(
                "SELECT u.id, u.email, u.plan, u.created_at, u.last_seen, "
                "  (SELECT COUNT(*) FROM orgs WHERE owner_id=u.id) AS org_count "
                "FROM users u ORDER BY u.last_seen DESC"
            ).fetchall()
            users = [{
                "id": r["id"], "email": r["email"], "plan": r["plan"],
                "created_at": r["created_at"], "last_seen": r["last_seen"],
                "org_count": r["org_count"],
            } for r in rows]
            return self._json({"users": users, "count": len(users)})

        # ── Admin: DB query (read-only) ───────────────────────────────────────
        if path == "/api/gateway/admin/query" and self.command == "POST":
            if not ADMIN_EMAILS or user_email not in ADMIN_EMAILS:
                return self._json({"error": "forbidden"}, 403)
            body = self._read_body()
            sql = body.get("sql", "").strip()
            if not sql:
                return self._json({"error": "missing sql"}, 400)
            # Block writes
            first_word = sql.split()[0].upper() if sql.split() else ""
            if first_word not in ("SELECT", "PRAGMA", "EXPLAIN"):
                return self._json({"error": "read-only queries only"}, 403)
            try:
                rows = db.execute(sql).fetchall()
                result = [dict(r) for r in rows]
                return self._json({"rows": result, "count": len(result)})
            except Exception as e:
                return self._json({"error": str(e)}, 400)

        # ── Determine target container via active org ─────────────────────────
        active_org = _active_org_id()
        org_data = db.execute("SELECT id, port, plan, trial_ends_at FROM orgs WHERE id=?", (active_org,)).fetchone()
        if not org_data or not org_data["port"]:
            return self._json({"error": "workspace not found"}, 404)
        target_org_id = org_data["id"]
        target_port = org_data["port"]

        # ── Hard gate: block expired free-plan users ─────────────────────────
        # Allow through: pro users, users still in trial, billing/gateway API calls, admins
        if org_data["plan"] != "pro" and not (ADMIN_EMAILS and user_email in ADMIN_EMAILS):
            trial_end = org_data["trial_ends_at"] or 0
            if trial_end < now:
                # Trial expired — allow only billing and gateway endpoints
                _allowed_prefixes = ("/api/stripe/", "/api/gateway/", "/api/cloud-")
                if not any(path.startswith(p) for p in _allowed_prefixes):
                    accept = self.headers.get("Accept", "")
                    if "text/html" in accept or not path.startswith("/api/"):
                        return self._html(_UPGRADE_HTML)
                    return self._json({"error": "trial_expired", "upgrade_url": "/upgrade"}, 402)

        # Refresh user's last_seen
        db.execute("UPDATE users SET last_seen=? WHERE id=?", (now, user_id))
        db.commit()

        # Wake target container if needed
        if not container_healthy(target_org_id):
            try:
                start_container(target_org_id, target_port)
            except Exception as e:
                return self._json({"error": f"failed to start instance: {e}"}, 503)

        proxy(self, target_port, path, qs, user_email=user_email, user_id=target_org_id)

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
