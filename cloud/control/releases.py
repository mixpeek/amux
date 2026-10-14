"""Staged release rollout (docs/design/cloud-self-service.md, 3.1 and M12).

A release reaches servers in three stages: the synthetic customer (stage 0), then
about 10% of servers chosen by a stable hash of their id (stage 1), then all
(stage 2). A stage advances after 24 hours with no rollback in it. Two rollbacks
of one release halt it and alert; the deploy freeze stops offering any release.
The fleet agent verifies the signature itself, so a compromised control plane can
offer a bad URL but cannot make a server run an unsigned binary.
"""
import hashlib, time

SOAK = 24 * 3600
CANARY_PERCENT = 10
HALT_AFTER_ROLLBACKS = 2


def publish(store, version, url, sha256, sig_url, now=None):
    now = now or time.time()
    store.run("INSERT OR IGNORE INTO releases(version,url,sha256,sig_url,stage,stage_started_at,created_at) "
              "VALUES(?,?,?,?,0,?,?)", version, url, sha256, sig_url, now, now)
    store.event("release.published", version=version)


def current(store):
    return store.one("SELECT * FROM releases WHERE halted=0 ORDER BY created_at DESC LIMIT 1")


def in_canary(server_id):
    return int(hashlib.sha256(server_id.encode()).hexdigest(), 16) % 100 < CANARY_PERCENT


def offer_for(store, srv, now=None):
    """The release this server should run now, or None."""
    if store.flag("deploy_freeze") == "1":
        return None
    rel = current(store)
    if not rel or srv.get("version") == rel["version"]:
        return None
    eligible = (srv.get("synthetic") == 1 or
                (rel["stage"] >= 1 and in_canary(srv["id"])) or
                rel["stage"] >= 2)
    if not eligible:
        return None
    return {"version": rel["version"], "url": rel["url"], "sha256": rel["sha256"], "sig_url": rel["sig_url"]}


def report(store, srv, rep, now=None):
    """The fleet agent's release_report: {version, outcome}."""
    now = now or time.time()
    if not rep or not rep.get("version"):
        return
    outcome = "rolled_back" if rep.get("outcome") == "rolled_back" else (
        "installed" if rep.get("outcome") == "installed" else "refused")
    fresh = store.one("SELECT 1 x FROM release_reports WHERE server_id=? AND version=?", srv["id"], rep["version"]) is None
    store.run("INSERT OR REPLACE INTO release_reports(server_id,version,outcome,ts) VALUES(?,?,?,?)",
              srv["id"], rep["version"], outcome, now)
    if fresh and outcome in ("rolled_back", "refused"):
        store.run("UPDATE releases SET rollbacks=rollbacks+1 WHERE version=?", rep["version"])
        rel = store.one("SELECT * FROM releases WHERE version=?", rep["version"])
        store.event("release.rollback", srv["org_id"], version=rep["version"], server=srv["id"], outcome=outcome)
        if rel and rel["rollbacks"] >= HALT_AFTER_ROLLBACKS and not rel["halted"]:
            store.run("UPDATE releases SET halted=1 WHERE version=?", rep["version"])
            store.alert("release_halted", f"release {rep['version']} halted after {rel['rollbacks']} rollbacks or refusals")


def advance(store, now=None):
    """Move the current release one stage on when its stage has soaked 24 hours cleanly."""
    now = now or time.time()
    rel = current(store)
    if not rel or rel["stage"] >= 2 or now - rel["stage_started_at"] < SOAK:
        return None
    installed = store.one("SELECT COUNT(*) n FROM release_reports WHERE version=? AND outcome='installed'",
                          rel["version"])["n"]
    if rel["stage"] == 0 and installed == 0:
        return None          # the synthetic server has not taken it yet
    if rel["rollbacks"]:
        return None
    store.run("UPDATE releases SET stage=stage+1, stage_started_at=? WHERE version=?", now, rel["version"])
    store.event("release.stage", version=rel["version"], stage=rel["stage"] + 1)
    return rel["stage"] + 1
