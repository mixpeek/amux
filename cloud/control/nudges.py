"""Lifecycle messages, the price guard and plan upgrades
(docs/design/cloud-self-service.md M6, M7, M10, M13, J15).

The fleet agent reports counts only, never content: whether any model is
connected, how many worker turns ran, and CPU and memory pressure. Each message
goes out at most once per its window, deduped in the outbox.
"""
import json, time

import price_check

DAY = 86400.0


def record_usage(store, srv, usage, now):
    """usage from the heartbeat: {model_connected, turns_total, cpu_pct, mem_pct}."""
    if not isinstance(usage, dict):
        return
    prev = store.one("SELECT props FROM events WHERE name='usage' AND props LIKE ? ORDER BY ts DESC LIMIT 1",
                     f'%"server": "{srv["id"]}"%')
    prev_turns = json.loads(prev["props"]).get("turns_total", 0) if prev else 0
    turns = int(usage.get("turns_total") or 0)
    store.event("usage", srv["org_id"], _ts=now, server=srv["id"], model_connected=bool(usage.get("model_connected")),
                turns_total=turns, cpu_pct=usage.get("cpu_pct"), mem_pct=usage.get("mem_pct"))
    if turns > prev_turns:
        if prev_turns == 0:
            store.event("cloud.first_reply", srv["org_id"], _ts=now, server=srv["id"],
                        ms_from_ready=int((now - (srv["ready_at"] or now)) * 1000))
        store.event("cloud.value", srv["org_id"], _ts=now, server=srv["id"], turns=turns - prev_turns)


def _latest(store, srv, name):
    r = store.one("SELECT ts, props FROM events WHERE name=? AND props LIKE ? ORDER BY ts DESC LIMIT 1",
                  name, f'%"server": "{srv["id"]}"%')
    return (r["ts"], json.loads(r["props"])) if r else (None, {})


def sweep(store, now=None):
    """Hourly. Returns {template: count} queued this pass."""
    now = now or time.time()
    sent = {}
    for srv in store.all("SELECT s.*, o.email FROM servers s JOIN orgs o ON o.id=s.org_id WHERE s.status='ready'"):
        _, usage = _latest(store, srv, "usage")
        ready = srv["ready_at"] or now
        first_reply, _ = _latest(store, srv, "cloud.first_reply")
        last_value, _ = _latest(store, srv, "cloud.value")
        # M6: set-up stalled
        if not usage.get("model_connected") and now - ready > 1800:
            sent["setup_model"] = sent.get("setup_model", 0) + _queue(store, srv, "setup_model", f"m6a:{srv['id']}")
        elif usage.get("model_connected") and not first_reply and now - ready > DAY:
            sent["setup_first_task"] = sent.get("setup_first_task", 0) + _queue(store, srv, "setup_first_task", f"m6b:{srv['id']}")
        # M7: quiet for a week, at most one nudge per 14 days
        quiet_since = last_value or first_reply
        if quiet_since and now - quiet_since > 7 * DAY:
            sent["inactive"] = sent.get("inactive", 0) + _queue(store, srv, "inactive", f"m7:{srv['id']}:{int(now // (14 * DAY))}")
        # M10: pressure for three days
        recent = [json.loads(r["props"]) for r in store.all(
            "SELECT props FROM events WHERE name='usage' AND ts>? AND props LIKE ?", now - 3 * DAY, f'%"server": "{srv["id"]}"%')]
        if len(recent) >= 48 and all(max(u.get("cpu_pct") or 0, u.get("mem_pct") or 0) >= 80 for u in recent) and \
                (store.one("SELECT MIN(ts) t FROM events WHERE name='usage' AND props LIKE ?", f'%"server": "{srv["id"]}"%')["t"] or now) < now - 3 * DAY:
            sent["upgrade_suggestion"] = sent.get("upgrade_suggestion", 0) + _queue(
                store, srv, "upgrade_suggestion", f"m10:{srv['id']}:{int(now // (30 * DAY))}")
    return sent


def _queue(store, srv, template, dedupe):
    before = store.one("SELECT COUNT(*) n FROM outbox WHERE dedupe=?", dedupe)["n"]
    store.enqueue_email(srv["email"], template, dedupe=dedupe, slug=srv["slug"], plan=srv["plan"])
    return 1 if not before else 0


def price_guard(store, plans, hetzner_prices, now=None):
    """M13, daily: any plan under guard_multiple x cost in any location alerts the owner; prices never change here."""
    problems = price_check.check_margin(plans, hetzner_prices)
    if problems:
        store.alert("price_guard", "; ".join(problems))
    store.event("price_guard", ok=not problems, problems=problems)
    return problems


def upgrade_if_plan_changed(store, infra, plans, sub_id, new_plan, now=None):
    """J15: a plan raised in the billing portal resizes the server to the larger type (about a minute down).
    Downgrades need export and import onto a smaller disk and are not automatic (plan 8.1)."""
    now = now or time.time()
    order = [p["id"] for p in plans["plans"]]
    srv = store.one("SELECT * FROM servers WHERE subscription_id=? AND status='ready'", sub_id)
    if not srv or new_plan not in order or new_plan == srv["plan"]:
        return None
    if order.index(new_plan) < order.index(srv["plan"]):
        store.event("cloud.downgrade_requested", srv["org_id"], server=srv["id"], to=new_plan)
        org = store.one("SELECT email FROM orgs WHERE id=?", srv["org_id"])
        store.enqueue_email(org["email"], "downgrade_manual", dedupe=f"down:{srv['id']}:{new_plan}", slug=srv["slug"], plan=new_plan)
        return "downgrade_needs_move"
    from store import new_id
    if store.one("SELECT 1 x FROM resizes WHERE server_id=? AND state='pending'", srv["id"]):
        return "already_pending"
    store.run("INSERT INTO resizes(id,server_id,to_plan,state,created_at) VALUES(?,?,?,?,?)",
              new_id("rsz"), srv["id"], new_plan, "pending", now)
    org = store.one("SELECT email FROM orgs WHERE id=?", srv["org_id"])
    store.enqueue_email(org["email"], "upgrade_scheduled", dedupe=f"up:{srv['id']}:{new_plan}", slug=srv["slug"], plan=new_plan)
    return "upgrade_pending"


def resize_tick(store, infra, plans, now=None):
    """Run pending upgrades one at a time: power off, change type with the larger disk, power on.
    Hetzner refuses a type change on a running server; a failure powers the old type back on."""
    now = now or time.time()
    job = store.one("SELECT r.*, s.hetzner_id, s.plan, s.org_id, s.slug FROM resizes r JOIN servers s ON s.id=r.server_id "
                    "WHERE r.state='pending' ORDER BY r.created_at LIMIT 1")
    if not job:
        return None
    stype = next(p["server_type"] for p in plans["plans"] if p["id"] == job["to_plan"])
    try:
        infra.resize(job["hetzner_id"], stype)
    except Exception as e:
        store.run("UPDATE resizes SET state='failed', detail=?, finished_at=? WHERE id=?", str(e)[:300], now, job["id"])
        store.alert("upgrade_failed", f"{job['slug']}: upgrade to {job['to_plan']} failed and the server was powered back on: {e}")
        return "failed"
    store.run("UPDATE resizes SET state='done', finished_at=? WHERE id=?", now, job["id"])
    store.run("UPDATE servers SET plan=? WHERE id=?", job["to_plan"], job["server_id"])
    store.event("cloud.upgraded", job["org_id"], server=job["server_id"], frm=job["plan"], to=job["to_plan"])
    return "upgraded"
