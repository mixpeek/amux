"""Fleet API: what a customer server tells the control plane, and what it gets back
(docs/design/cloud-self-service.md, 3.1, J7, 8.2, 8.5).

Each server authenticates with its own fleet secret, so it can only ever speak
for itself. Everything here is outbound from the server, which is why updates,
backups and lifecycle keep working when a customer turns on tailnet-only.
"""
import hmac, json, time

import abuse, drill, nudges, provision, releases
import os
from tokens import new_secret

INVITE_EMAILS_PER_DAY = 20


def authenticate(store, server_id, bearer):
    if server_id.startswith("drl_"):
        d = drill.credentials_for(store, server_id, bearer or "")
        return dict(d, kind="drill") if d else None
    srv = store.one("SELECT * FROM servers WHERE id=?", server_id)
    if not srv or not bearer or not hmac.compare_digest(srv["fleet_secret"], bearer):
        return None
    if srv["status"] in ("deleted", "failed"):
        return None
    return srv


def drill_heartbeat(store, infra, d, body, now=None):
    """A restore-drill server: hand it the source bucket's credentials, then take its report."""
    if body.get("drill_result"):
        return {"result": drill.report(store, infra, d["id"], body["drill_result"], now)}
    src = store.one("SELECT * FROM servers WHERE id=?", d["server_id"])
    creds = dict(infra.backup_bucket(src["slug"], src["id"]), restic_password=src["restic_password"])
    return {"restore_from": creds, "commands": []}


def heartbeat(store, infra, srv, body, now=None):
    """body: {version, last_backup_at, disk_used_pct, csr?, serving: bool, door_ok, amux_ok, backup_fingerprint?}."""
    now = now or time.time()
    if srv.get("kind") == "drill":
        return drill_heartbeat(store, infra, srv, body, now)
    if isinstance(body.get("backup_fingerprint"), dict):
        store.run("UPDATE servers SET backup_fingerprint=? WHERE id=?", json.dumps(body["backup_fingerprint"]), srv["id"])
    tn = body.get("tailnet_url")
    store.run("UPDATE servers SET last_heartbeat=?, version=?, last_backup_at=COALESCE(?, last_backup_at), tailnet_url=? WHERE id=?",
              now, body.get("version"), body.get("last_backup_at"),
              tn if isinstance(tn, str) and tn.startswith("https://") and tn.endswith(".ts.net") else None, srv["id"])
    reply = {"commands": [], "tailnet_only": bool(srv["tailnet_only"])}
    if body.get("csr"):
        try:
            reply["origin_certificate"] = infra.origin_certificate(srv["slug"], body["csr"])
            store.event("cloud.origin_cert", srv["org_id"], server=srv["id"])
        except Exception as e:
            store.event("cloud.origin_cert_failed", srv["org_id"], server=srv["id"], error=str(e)[:200])
    if srv["status"] == "provisioning" and body.get("serving") and body.get("last_backup_at"):
        provision.mark_ready(store, srv["id"], now)
        reply["ready"] = True
    if body.get("need_backup_creds") or (srv["status"] == "provisioning" and not srv["backup_bucket"]):
        try:
            creds = infra.backup_bucket(srv["slug"], srv["id"])
            restic_pw = srv["restic_password"] or new_secret()
            store.run("UPDATE servers SET backup_bucket=?, restic_password=? WHERE id=?", creds["bucket"], restic_pw, srv["id"])
            reply["backup_credentials"] = dict(creds, restic_password=restic_pw)
        except Exception as e:
            store.event("cloud.backup_creds_failed", srv["org_id"], server=srv["id"], error=str(e)[:200])
    nudges.record_usage(store, srv, body.get("usage"), now)
    if (body.get("disk_used_pct") or 0) >= 95:
        org = store.one("SELECT email FROM orgs WHERE id=?", srv["org_id"])
        store.enqueue_email(org["email"], "disk_full", dedupe=f"m3:{srv['id']}:{int(now // 86400)}", slug=srv["slug"])
        store.event("cloud.disk_full", srv["org_id"], server=srv["id"], pct=body.get("disk_used_pct"))
    if body.get("abuse_signals"):
        ips = [ip for ip in os.environ.get("CONTROL_PUBLIC_IPS", "").split(",") if ip]
        abuse.handle(store, infra, srv, body["abuse_signals"], ips, now)
    if abuse.stop_workers_command(store, srv):
        reply["commands"].append("stop_workers")
    releases.report(store, srv, body.get("release_report"), now)
    offer = releases.offer_for(store, dict(srv, version=body.get("version")), now)
    if offer:
        reply["release"] = offer
    return reply


def invite_email(store, srv, body, now=None):
    """8.2: the customer server asks us to send email 7. Only to the invite's own address, 20 a day."""
    now = now or time.time()
    to, invite_url = (body.get("email") or "").strip().lower(), body.get("invite_url") or ""
    if "@" not in to or not invite_url.startswith(f"https://cloud.amux.io/invite/"):
        return 400, {"error": "email and a cloud.amux.io invite_url are required"}
    sent = store.one("SELECT COUNT(*) n FROM outbox WHERE template='invited' AND ts>? AND params LIKE ?",
                     now - 86400, f'%"server": "{srv["id"]}"%')["n"]
    if sent >= INVITE_EMAILS_PER_DAY:
        return 429, {"error": "invite email limit reached for today"}
    store.enqueue_email(to, "invited", dedupe=f"invite:{invite_url}", ts=now, server=srv["id"], slug=srv["slug"],
                        invite_url=invite_url, inviter=body.get("inviter") or "")
    store.event("cloud.invite_sent", srv["org_id"], server=srv["id"])
    return 200, {"queued": True}
