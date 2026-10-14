"""Account deletion (docs/design/cloud-self-service.md J18, M19).

The person asked, so nothing is held back for recovery: servers, their snapshots,
DNS records and backup buckets go, the subscription ends now, the Apple token is
revoked and the account rows are removed. Invoices stay, because tax law keeps
them. Each step is recorded; a failed step is retried by the control plane's loop
until it succeeds, and the receipt lists every step once all are done.
"""
import json, time

from store import new_id


def plan_steps(store, user):
    steps = []
    if user["role"] == "owner":
        for srv in store.all("SELECT * FROM servers WHERE org_id=? AND status IN ('ready','provisioning','deleted','failed')",
                             user["org_id"]):
            if srv["hetzner_id"] and srv["status"] in ("ready", "provisioning"):
                steps.append({"kind": "server", "ref": srv["hetzner_id"], "label": f"server {srv['slug']}"})
            if srv["dns_record_id"]:
                steps.append({"kind": "dns", "ref": srv["dns_record_id"], "label": f"address {srv['slug']}.amux.cloud"})
            if srv["backup_bucket"]:
                steps.append({"kind": "bucket", "ref": srv["backup_bucket"], "label": f"backups of {srv['slug']}"})
            for a in store.all("SELECT snapshot_id FROM lifecycle_actions WHERE server_id=? AND snapshot_id IS NOT NULL",
                               srv["id"]):
                steps.append({"kind": "snapshot", "ref": a["snapshot_id"], "label": f"snapshot of {srv['slug']}"})
        for sub in store.all("SELECT id FROM subscriptions WHERE org_id=? AND status NOT IN ('canceled')", user["org_id"]):
            steps.append({"kind": "subscription", "ref": sub["id"], "label": "subscription"})
    if user.get("apple_refresh"):
        steps.append({"kind": "apple", "ref": user["apple_refresh"], "label": "Sign in with Apple link"})
    steps.append({"kind": "account", "ref": user["id"], "label": "account and sign-in"})
    return [dict(s, state="pending") for s in steps]


def request(store, user, now=None):
    now = now or time.time()
    existing = store.one("SELECT * FROM deletions WHERE user_id=? AND done_at IS NULL", user["id"])
    if existing:
        return existing["id"]
    did = new_id("del")
    store.run("INSERT INTO deletions(id,user_id,email,created_at,steps) VALUES(?,?,?,?,?)", did, user["id"], user["email"],
              now, json.dumps(plan_steps(store, user)))
    store.event("cloud.deletion_requested", user["org_id"], deletion=did)
    return did


def run(store, billing, infra, revoke_apple=None, now=None):
    """Advance every open deletion. Returns the number finished in this pass."""
    now = now or time.time()
    finished = 0
    for d in store.all("SELECT * FROM deletions WHERE done_at IS NULL"):
        steps = json.loads(d["steps"])
        for s in steps:
            if s["state"] == "done":
                continue
            try:
                k, ref = s["kind"], s["ref"]
                if k == "server":
                    try:
                        infra.delete_server(ref)
                    except KeyError:
                        pass                                  # already gone
                elif k == "dns":
                    infra.delete_dns(ref)
                elif k == "bucket":
                    infra.delete_bucket(ref)
                elif k == "snapshot":
                    infra.delete_image(ref)
                elif k == "subscription":
                    billing.cancel_now(ref)
                elif k == "apple":
                    if not revoke_apple:
                        raise RuntimeError("Apple key not configured; the token is kept until it can be revoked")
                    revoke_apple(ref)
                elif k == "account":
                    u = store.one("SELECT * FROM users WHERE id=?", ref)
                    if u:
                        store.run("UPDATE servers SET status='deleted' WHERE org_id=? AND ?='owner'", u["org_id"], u["role"])
                        store.run("DELETE FROM memberships WHERE user_id=?", ref)
                        store.run("DELETE FROM users WHERE id=?", ref)
                        if u["role"] == "owner":
                            store.run("UPDATE orgs SET email='deleted', owner_user_id=NULL WHERE id=?", u["org_id"])
                s["state"], s["done_at"] = "done", now
                s.pop("ref", None) if k == "apple" else None          # never keep the Apple token after use
            except Exception as e:
                s["state"], s["error"] = "retrying", str(e)[:200]
        store.run("UPDATE deletions SET steps=? WHERE id=?", json.dumps(steps), d["id"])
        if all(s["state"] == "done" for s in steps):
            store.run("UPDATE deletions SET done_at=? WHERE id=?", now, d["id"])
            store.enqueue_email(d["email"], "deleted", dedupe=f"deleted:{d['id']}", items=[s["label"] for s in steps])
            store.event("cloud.deleted", deletion=d["id"])
            finished += 1
    return finished
