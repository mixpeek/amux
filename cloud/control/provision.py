"""Provisioning (docs/design/cloud-self-service.md, J7 and M1).

One job per server, advanced by `tick()` from the control plane's loop:

  create  create the Hetzner server from the golden image in the first location
          that has capacity (capacity errors move to the next EU location)
  dns     a proxied Cloudflare A record <slug>.amux.cloud
  boot    wait for the fleet agent's first heartbeat; it sends a CSR, and the
          heartbeat reply carries the Origin CA certificate for that host
  ready   the agent reports door, amux-server and the first backup healthy

Over 15 minutes without ready: refund within the cap, email 2, an owner card,
and the half-built server is snapshot-free deleted (it never held customer data).
"""
import json, re, time

from store import new_id
from tokens import new_secret

PROVISION_TIMEOUT = 15 * 60
SLUG_RE = re.compile(r"[^a-z0-9]+")


def make_slug(store, email):
    base = SLUG_RE.sub("-", email.split("@")[0].lower()).strip("-")[:20] or "amux"
    slug, n = base, 1
    while store.one("SELECT 1 x FROM servers WHERE slug=?", slug) or slug in RESERVED:
        n += 1
        slug = f"{base}-{n}"
    return slug


RESERVED = {"www", "api", "cloud", "control", "admin", "status", "mail", "app", "amux"}


def start(store, org_id, subscription_id, plan, email, location=None, now=None):
    """Create the server row and its job. Idempotent per subscription."""
    now = now or time.time()
    existing = store.one("SELECT * FROM servers WHERE subscription_id=? AND status IN ('provisioning','ready')",
                         subscription_id)
    if existing:
        return existing
    sid = new_id("srv")
    store.run("INSERT INTO servers(id,org_id,subscription_id,slug,plan,location,status,fleet_secret,created_at) "
              "VALUES(?,?,?,?,?,?,?,?,?)", sid, org_id, subscription_id, make_slug(store, email), plan,
              location, "provisioning", new_secret(), now)
    store.run("INSERT INTO provision_jobs(id,server_id,step,status,started_at,updated_at) VALUES(?,?,?,?,?,?)",
              new_id("job"), sid, "create", "pending", now, now)
    store.event("cloud.provision_started", org_id, server=sid, plan=plan)
    return store.one("SELECT * FROM servers WHERE id=?", sid)


def user_data(server, control_url, owner_email):
    """cloud-init: hand the golden image its identity; the fleet agent does the rest."""
    env = {
        "AMUX_CONTROL_URL": control_url, "AMUX_SERVER_ID": server["id"], "AMUX_SLUG": server["slug"],
        "AMUX_FLEET_SECRET": server["fleet_secret"], "AMUX_OWNER_EMAIL": owner_email,
    }
    lines = "\n".join(f"{k}={json.dumps(v)}" for k, v in env.items())
    return ("#cloud-config\nwrite_files:\n  - path: /etc/amux/fleet.env\n    permissions: '0600'\n"
            "    content: |\n" + "".join(f"      {l}\n" for l in lines.splitlines()) +
            "runcmd:\n  - [systemctl, enable, --now, amux-fleet-agent.service]\n")


def tick(store, infra, plans, golden_image, control_url, now=None, refund=None):
    """Advance every unfinished job one step. Returns the number of jobs touched."""
    now = now or time.time()
    if store.flag("provisioning_freeze") == "1":
        return 0
    touched = 0
    for job in store.all("SELECT j.*, s.org_id, s.plan, s.slug, s.location, s.hetzner_id, s.ipv4, s.fleet_secret, "
                         "s.created_at srv_created, s.subscription_id FROM provision_jobs j "
                         "JOIN servers s ON s.id=j.server_id WHERE j.status IN ('pending','running')"):
        touched += 1
        if now - job["started_at"] > PROVISION_TIMEOUT:
            fail(store, infra, job, "not ready within 15 minutes", now, refund)
            continue
        try:
            _step(store, infra, plans, golden_image, control_url, job, now)
        except Exception as e:
            store.run("UPDATE provision_jobs SET attempts=attempts+1, error=?, updated_at=? WHERE id=?",
                      str(e)[:500], now, job["id"])
            store.event("cloud.provision_retry", job["org_id"], job=job["id"], step=job["step"], error=str(e)[:200])
    return touched


def _step(store, infra, plans, golden_image, control_url, job, now):
    from fakes import CapacityError
    plan = next(p for p in plans["plans"] if p["id"] == job["plan"])
    if job["step"] == "create":
        tried = json.loads(job["locations_tried"])
        order = [job["location"] or plans["default_location"]] + [l for l in plans["locations"]]
        order = [l for i, l in enumerate(order) if l and l not in order[:i] and l not in tried]
        if not order:
            raise RuntimeError("no EU location has capacity for " + plan["server_type"])
        loc = order[0]
        org = store.one("SELECT email FROM orgs WHERE id=?", job["org_id"])
        srv = store.one("SELECT * FROM servers WHERE id=?", job["server_id"])
        try:
            h = infra.create_server(f"amux-{job['slug']}", plan["server_type"], loc, golden_image,
                                    {"amux-cloud": "1", "org": job["org_id"], "server": job["server_id"]},
                                    user_data(srv, control_url, org["email"]))
        except CapacityError:
            store.run("UPDATE provision_jobs SET locations_tried=?, updated_at=? WHERE id=?",
                      json.dumps(tried + [loc]), now, job["id"])
            store.event("cloud.provision_capacity", job["org_id"], location=loc)
            return
        store.run("UPDATE servers SET hetzner_id=?, ipv4=?, location=? WHERE id=?", h["id"], h["ipv4"], loc, job["server_id"])
        _advance(store, job, "dns", now)
    elif job["step"] == "dns":
        rid = infra.create_dns(job["slug"], job["ipv4"])
        store.run("UPDATE servers SET dns_record_id=? WHERE id=?", rid, job["server_id"])
        _advance(store, job, "boot", now)
    # "boot" advances when the fleet agent's heartbeat says it is serving (fleet.heartbeat).


def _advance(store, job, step, now):
    store.run("UPDATE provision_jobs SET step=?, status='running', updated_at=? WHERE id=?", step, now, job["id"])


def mark_ready(store, server_id, now=None):
    now = now or time.time()
    srv = store.one("SELECT * FROM servers WHERE id=?", server_id)
    if not srv or srv["status"] != "provisioning":
        return False
    store.run("UPDATE servers SET status='ready', ready_at=? WHERE id=?", now, server_id)
    store.run("UPDATE provision_jobs SET step='ready', status='done', updated_at=? WHERE server_id=? AND status!='done'",
              now, server_id)
    store.event("cloud.ready", srv["org_id"], server=server_id, ms_from_payment=int((now - srv["created_at"]) * 1000))
    return True


def fail(store, infra, job, why, now, refund=None):
    """M1 escalation: the half-built server never held customer data, so it is removed outright."""
    store.run("UPDATE provision_jobs SET status='failed', error=?, updated_at=? WHERE id=?", why, now, job["id"])
    store.run("UPDATE servers SET status='failed' WHERE id=?", job["server_id"])
    srv = store.one("SELECT * FROM servers WHERE id=?", job["server_id"])
    for cleanup in ((lambda: infra.delete_server(srv["hetzner_id"])) if srv["hetzner_id"] else None,
                    (lambda: infra.delete_dns(srv["dns_record_id"])) if srv["dns_record_id"] else None):
        if cleanup:
            try:
                cleanup()
            except Exception as e:
                store.event("cloud.provision_cleanup_failed", srv["org_id"], error=str(e)[:200])
    refunded = refund(store, srv) if refund else None
    org = store.one("SELECT email FROM orgs WHERE id=?", srv["org_id"])
    if org:
        store.enqueue_email(org["email"], "provision_failed", dedupe=f"provfail:{srv['id']}", slug=srv["slug"],
                            refunded=bool(refunded))
    store.alert("provision_failed", f"{srv['slug']}: {why}; refund {'issued' if refunded else 'queued for owner'}")
    store.event("cloud.provision_failed", srv["org_id"], server=srv["id"], why=why, refunded=bool(refunded))
