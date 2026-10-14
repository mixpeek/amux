"""Support requests and replies (docs/design/cloud-self-service.md J19, M8).

A customer asks from cloud.amux.io ("Something wrong?"). The control plane keeps
the request with a diagnostics snapshot the fleet agent last sent (health,
version, disk, backup age, error counts, never content) and hands it to the
Operator's support worker through the admin API. The worker drafts an answer.

Replies reach the customer only one of two ways, chosen by the owner:
  support_auto_reply = 0 (default)  each draft waits for the owner's approval
  support_auto_reply = 1            drafts are sent as written
A draft that proposes changing the customer's server is never applied here; the
customer approves such a fix on their own server.
"""
import json, time

from store import new_id

SCHEMA = """
CREATE TABLE IF NOT EXISTS support (
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, user_id TEXT NOT NULL, email TEXT NOT NULL, server_id TEXT,
  message TEXT NOT NULL, diagnostics TEXT, created_at REAL NOT NULL,
  draft TEXT, drafted_at REAL, state TEXT NOT NULL DEFAULT 'open', sent_at REAL);
"""


def ensure(store):
    with store.conn() as c:
        c.executescript(SCHEMA)
    if store.one("SELECT value FROM flags WHERE name='support_auto_reply'") is None:
        store.set_flag("support_auto_reply", "0")


def open_request(store, user, message, server=None, now=None):
    now = now or time.time()
    message = (message or "").strip()
    if not message or len(message) > 5000:
        raise ValueError("describe the problem in under 5,000 characters")
    recent = store.one("SELECT COUNT(*) n FROM support WHERE user_id=? AND created_at>?", user["id"], now - 3600)["n"]
    if recent >= 5:
        raise ValueError("five requests in the last hour; we are working on them")
    diag = None
    if server:
        last = store.one("SELECT props FROM events WHERE name='usage' AND props LIKE ? ORDER BY ts DESC LIMIT 1",
                         f'%"server": "{server["id"]}"%')
        diag = {"status": server["status"], "version": server["version"], "last_heartbeat": server["last_heartbeat"],
                "last_backup_at": server["last_backup_at"], "plan": server["plan"],
                "usage": json.loads(last["props"]) if last else None}
    sid = new_id("sup")
    store.run("INSERT INTO support(id,org_id,user_id,email,server_id,message,diagnostics,created_at) VALUES(?,?,?,?,?,?,?,?)",
              sid, user["org_id"], user["id"], user["email"], server["id"] if server else None, message,
              json.dumps(diag) if diag else None, now)
    store.event("cloud.support_opened", user["org_id"], request=sid)
    return sid


def queue(store):
    """What the Operator's support worker reads: open requests without a draft."""
    return store.all("SELECT id, message, diagnostics, created_at FROM support WHERE state='open' AND draft IS NULL ORDER BY created_at")


def draft(store, request_id, text, now=None):
    now = now or time.time()
    row = store.one("SELECT * FROM support WHERE id=?", request_id)
    if not row or row["state"] != "open":
        raise ValueError("no open request with that id")
    store.run("UPDATE support SET draft=?, drafted_at=? WHERE id=?", text.strip(), now, request_id)
    if store.flag("support_auto_reply") == "1":
        return send(store, request_id, now)
    store.alert("support_draft", f"support reply to {row['email']} is waiting for your approval ({request_id})")
    return "awaiting_approval"


def send(store, request_id, now=None):
    now = now or time.time()
    row = store.one("SELECT * FROM support WHERE id=?", request_id)
    if not row or not row["draft"]:
        raise ValueError("nothing to send")
    store.enqueue_email(row["email"], "support_reply", dedupe=f"support:{request_id}", text=row["draft"])
    store.run("UPDATE support SET state='answered', sent_at=? WHERE id=?", now, request_id)
    store.event("cloud.support_answered", row["org_id"], request=request_id,
                minutes=round((now - row["created_at"]) / 60, 1))
    return "sent"
