"""Weekly restore drill (docs/design/cloud-self-service.md 3.1, M4, T-OPS-3).

For each live server once a week: boot a throwaway server from the golden image in
drill mode, let its fleet agent restore the database (litestream) and files
(restic) from the live server's bucket, count rows per table and fingerprint the
file list, and compare with the fingerprint the live server reported after its
last backup. Pass or fail is recorded; the drill server is deleted either way and
the live server is never touched. Two failures in a row escalate (M4).
"""
import json, time

from store import new_id
from tokens import new_secret

WEEK = 7 * 86400.0
DRILL_TIMEOUT = 45 * 60


def due(store, now):
    out = []
    for s in store.all("SELECT * FROM servers WHERE status='ready' AND backup_bucket IS NOT NULL AND backup_fingerprint IS NOT NULL"):
        last = store.one("SELECT MAX(started_at) t FROM drills WHERE server_id=?", s["id"])["t"]
        if not last or now - last >= WEEK:
            out.append(s)
    return out


def start(store, infra, srv, golden_image, control_url, now=None):
    now = now or time.time()
    did, secret = new_id("drl"), new_secret()
    env = {"AMUX_CONTROL_URL": control_url, "AMUX_SERVER_ID": did, "AMUX_FLEET_SECRET": secret, "AMUX_MODE": "drill",
           "AMUX_DRILL_OF": srv["id"], "AMUX_SLUG": f"drill-{srv['slug']}"}
    user_data = ("#cloud-config\nwrite_files:\n  - path: /etc/amux/fleet.env\n    permissions: '0600'\n    content: |\n" +
                 "".join(f"      {k}={json.dumps(v)}\n" for k, v in env.items()) +
                 "runcmd:\n  - [systemctl, enable, --now, amux-fleet-agent.service]\n")
    h = infra.create_server(f"amux-drill-{srv['slug']}", "cx23", srv["location"] or "fsn1", golden_image,
                            {"amux-cloud": "1", "purpose": "restore-drill", "drill": did}, user_data)
    store.run("INSERT INTO drills(id,server_id,hetzner_id,fleet_secret,started_at,expected) VALUES(?,?,?,?,?,?)",
              did, srv["id"], h["id"], secret, now, srv["backup_fingerprint"])
    store.event("drill.started", srv["org_id"], server=srv["id"], drill=did)
    return did


def credentials_for(store, drill_id, secret):
    """The drill server asks for the source bucket's credentials with its own one-time secret."""
    d = store.one("SELECT * FROM drills WHERE id=? AND finished_at IS NULL", drill_id)
    if not d or d["fleet_secret"] != secret:
        return None
    return d


def report(store, infra, drill_id, got, now=None):
    """got: the restored fingerprint {tables: {name: rows}, files_sha256, files}. Compares, records, deletes the drill server."""
    now = now or time.time()
    d = store.one("SELECT * FROM drills WHERE id=?", drill_id)
    if not d or d["finished_at"]:
        return None
    want = json.loads(d["expected"] or "{}")
    problems = []
    for t, n in (want.get("tables") or {}).items():
        have = (got.get("tables") or {}).get(t)
        if have is None:
            problems.append(f"table {t} missing")
        elif have < n:
            problems.append(f"table {t}: {have} rows restored, {n} expected")
    if want.get("files_sha256") and got.get("files_sha256") != want.get("files_sha256"):
        problems.append(f"files differ ({got.get('files')} restored, {want.get('files')} expected)")
    result = "pass" if not problems else "fail"
    finish(store, infra, d, result, "; ".join(problems) or "restored rows and files match", now)
    return result


def finish(store, infra, d, result, detail, now):
    try:
        infra.delete_server(d["hetzner_id"])
    except Exception as e:
        detail += f" (drill server cleanup failed: {e}; the orphan report will list it)"
    store.run("UPDATE drills SET result=?, detail=?, finished_at=? WHERE id=?", result, detail, now, d["id"])
    srv = store.one("SELECT * FROM servers WHERE id=?", d["server_id"])
    store.event("drill.finished", srv["org_id"] if srv else None, server=d["server_id"], result=result, detail=detail)
    if result != "pass":
        prev = store.all("SELECT result FROM drills WHERE server_id=? AND finished_at IS NOT NULL ORDER BY started_at DESC LIMIT 2",
                         d["server_id"])
        if len(prev) == 2 and all(p["result"] != "pass" for p in prev):
            store.alert("restore_drill_failed", f"{srv['slug'] if srv else d['server_id']}: two restore drills failed in a row: {detail}")


def sweep(store, infra, golden_image, control_url, now=None):
    """Start due drills and time out stuck ones. Run hourly."""
    now = now or time.time()
    for d in store.all("SELECT * FROM drills WHERE finished_at IS NULL AND started_at<?", now - DRILL_TIMEOUT):
        finish(store, infra, d, "fail", "the drill server did not report within 45 minutes", now)
    started = 0
    for srv in due(store, now)[:5]:                    # at most five drills an hour across the fleet
        start(store, infra, srv, golden_image, control_url, now)
        started += 1
    return started
