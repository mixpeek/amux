"""SQLite store for the amux Cloud control plane (docs/design/cloud-self-service.md, section 3).

One file, WAL mode, opened per thread. Every table the plan names lives here;
nothing else in the control plane keeps state.
"""
import json, os, sqlite3, threading, time, uuid

SCHEMA = """
CREATE TABLE IF NOT EXISTS orgs (
  id TEXT PRIMARY KEY, owner_user_id TEXT, email TEXT NOT NULL,
  stripe_customer_id TEXT UNIQUE, source TEXT, created_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS users (
  id TEXT PRIMARY KEY, email TEXT NOT NULL, email_verified INTEGER NOT NULL DEFAULT 0,
  org_id TEXT, created_at REAL NOT NULL, role TEXT NOT NULL DEFAULT 'owner',
  apple_sub TEXT UNIQUE, apple_refresh TEXT);
CREATE TABLE IF NOT EXISTS deletions (
  id TEXT PRIMARY KEY, user_id TEXT NOT NULL, email TEXT NOT NULL, created_at REAL NOT NULL,
  steps TEXT NOT NULL, done_at REAL);
CREATE UNIQUE INDEX IF NOT EXISTS users_email ON users(lower(email));
CREATE TABLE IF NOT EXISTS signups (
  checkout_session_id TEXT PRIMARY KEY, org_id TEXT NOT NULL, plan TEXT NOT NULL,
  server_id TEXT, created_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS subscriptions (
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, plan TEXT NOT NULL, status TEXT NOT NULL,
  current_period_end REAL, cancel_at_period_end INTEGER NOT NULL DEFAULT 0,
  failure_started_at REAL, updated_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS ledger (
  invoice_id TEXT PRIMARY KEY, org_id TEXT NOT NULL, subscription_id TEXT,
  amount_cents INTEGER NOT NULL, paid_at REAL NOT NULL, payment_intent TEXT);
CREATE TABLE IF NOT EXISTS refunds (
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, payment_intent TEXT, amount_cents INTEGER NOT NULL,
  reason TEXT NOT NULL, state TEXT NOT NULL, created_at REAL NOT NULL, decided_at REAL);
CREATE TABLE IF NOT EXISTS stripe_events (id TEXT PRIMARY KEY, type TEXT NOT NULL, ts REAL NOT NULL);
CREATE TABLE IF NOT EXISTS servers (
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, subscription_id TEXT, slug TEXT UNIQUE NOT NULL,
  plan TEXT NOT NULL, location TEXT, hetzner_id INTEGER, ipv4 TEXT, dns_record_id TEXT,
  status TEXT NOT NULL, fleet_secret TEXT NOT NULL, created_at REAL NOT NULL, ready_at REAL,
  last_heartbeat REAL, last_backup_at REAL, version TEXT, tailnet_only INTEGER NOT NULL DEFAULT 0,
  synthetic INTEGER NOT NULL DEFAULT 0, backup_bucket TEXT, restic_password TEXT, tailnet_url TEXT,
  backup_fingerprint TEXT);
CREATE TABLE IF NOT EXISTS releases (
  version TEXT PRIMARY KEY, url TEXT NOT NULL, sha256 TEXT NOT NULL, sig_url TEXT NOT NULL,
  stage INTEGER NOT NULL DEFAULT 0, stage_started_at REAL NOT NULL, created_at REAL NOT NULL,
  rollbacks INTEGER NOT NULL DEFAULT 0, halted INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS release_reports (
  server_id TEXT NOT NULL, version TEXT NOT NULL, outcome TEXT NOT NULL, ts REAL NOT NULL,
  PRIMARY KEY(server_id, version));
CREATE TABLE IF NOT EXISTS provision_jobs (
  id TEXT PRIMARY KEY, server_id TEXT NOT NULL, step TEXT NOT NULL, status TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0, error TEXT, started_at REAL NOT NULL, updated_at REAL NOT NULL,
  locations_tried TEXT NOT NULL DEFAULT '[]');
CREATE TABLE IF NOT EXISTS lifecycle_actions (
  id TEXT PRIMARY KEY, server_id TEXT NOT NULL, kind TEXT NOT NULL, reason TEXT NOT NULL,
  state TEXT NOT NULL, shadow INTEGER NOT NULL, proposed_at REAL NOT NULL, hold_until REAL NOT NULL,
  snapshot_id TEXT, evidence TEXT NOT NULL DEFAULT '{}', decided_at REAL,
  replay_at REAL, replay_verdict TEXT);
CREATE TABLE IF NOT EXISTS resizes (
  id TEXT PRIMARY KEY, server_id TEXT NOT NULL, to_plan TEXT NOT NULL, state TEXT NOT NULL, created_at REAL NOT NULL,
  detail TEXT, finished_at REAL);
CREATE TABLE IF NOT EXISTS drills (
  id TEXT PRIMARY KEY, server_id TEXT NOT NULL, hetzner_id INTEGER, fleet_secret TEXT NOT NULL, started_at REAL NOT NULL,
  expected TEXT, result TEXT, detail TEXT, finished_at REAL);
CREATE TABLE IF NOT EXISTS device_codes (
  device_code TEXT PRIMARY KEY, user_code TEXT UNIQUE NOT NULL, created_at REAL NOT NULL, expires_at REAL NOT NULL,
  user_id TEXT, state TEXT NOT NULL DEFAULT 'pending');
CREATE TABLE IF NOT EXISTS memberships (
  user_id TEXT NOT NULL, server_id TEXT NOT NULL, created_at REAL NOT NULL, PRIMARY KEY(user_id, server_id));
CREATE TABLE IF NOT EXISTS flags (name TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS waitlist (email TEXT PRIMARY KEY, source TEXT, created_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, name TEXT NOT NULL,
  org_id TEXT, props TEXT NOT NULL DEFAULT '{}');
CREATE TABLE IF NOT EXISTS outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, to_addr TEXT NOT NULL,
  template TEXT NOT NULL, params TEXT NOT NULL, dedupe TEXT UNIQUE, sent_at REAL, error TEXT);
CREATE TABLE IF NOT EXISTS alerts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL, kind TEXT NOT NULL, message TEXT NOT NULL,
  acknowledged INTEGER NOT NULL DEFAULT 0);
"""

# Kill switches (plan 11.2) and the lifecycle shadow flag (plan 3.2 rule 8).
DEFAULT_FLAGS = {
    "lifecycle_freeze": "0",
    "deploy_freeze": "0",
    "provisioning_freeze": "0",
    "spend_freeze": "0",
    "lifecycle_shadow": "1",
}


class Store:
    def __init__(self, path):
        self.path = path
        self._local = threading.local()
        with self.conn() as c:
            c.executescript(SCHEMA)
            for k, v in DEFAULT_FLAGS.items():
                c.execute("INSERT OR IGNORE INTO flags(name,value,updated_at) VALUES(?,?,?)", (k, v, time.time()))

    def conn(self):
        c = getattr(self._local, "c", None)
        if c is None:
            c = sqlite3.connect(self.path, timeout=30, isolation_level=None)
            c.row_factory = sqlite3.Row
            c.execute("PRAGMA journal_mode=WAL")
            c.execute("PRAGMA foreign_keys=ON")
            self._local.c = c
        return _Tx(c)

    # ---- small helpers -------------------------------------------------
    def one(self, sql, *args):
        with self.conn() as c:
            r = c.execute(sql, args).fetchone()
            return dict(r) if r else None

    def all(self, sql, *args):
        with self.conn() as c:
            return [dict(r) for r in c.execute(sql, args).fetchall()]

    def run(self, sql, *args):
        with self.conn() as c:
            return c.execute(sql, args)

    def flag(self, name):
        r = self.one("SELECT value FROM flags WHERE name=?", name)
        return r["value"] if r else DEFAULT_FLAGS.get(name, "0")

    def set_flag(self, name, value):
        self.run("INSERT INTO flags(name,value,updated_at) VALUES(?,?,?) "
                 "ON CONFLICT(name) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                 name, str(value), time.time())

    def event(self, name, org_id=None, _ts=None, **props):
        self.run("INSERT INTO events(ts,name,org_id,props) VALUES(?,?,?,?)",
                 _ts or time.time(), name, org_id, json.dumps(props, sort_keys=True))

    def alert(self, kind, message):
        """Owner-facing alert. The sender (emails.py) delivers unacknowledged rows."""
        self.run("INSERT INTO alerts(ts,kind,message) VALUES(?,?,?)", time.time(), kind, message)
        print(f"[control] ALERT {kind}: {message}", flush=True)

    def enqueue_email(self, to, template, dedupe=None, ts=None, **params):
        """Idempotent on dedupe: the same lifecycle email is never queued twice."""
        self.run("INSERT OR IGNORE INTO outbox(ts,to_addr,template,params,dedupe) VALUES(?,?,?,?,?)",
                 ts or time.time(), to, template, json.dumps(params, sort_keys=True), dedupe)


class _Tx:
    """`with store.conn() as c:` gives the connection; BEGIN/COMMIT only when asked."""
    def __init__(self, c):
        self.c = c

    def __enter__(self):
        return self.c

    def __exit__(self, *exc):
        return False


def new_id(prefix):
    return f"{prefix}_{uuid.uuid4().hex[:16]}"


def open_store(path=None):
    path = path or os.environ.get("CONTROL_DB", "/var/amux/control.db")
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    return Store(path)
