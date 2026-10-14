#!/usr/bin/env python3
"""The owner's weekly report (docs/design/cloud-self-service.md section 9).

Every line is computed from the control plane's own tables. A line whose input
the control plane does not have yet says "not measured" rather than printing a
zero, so a quiet week and a blind week never look the same.

  report.py [--days 7]      print the report (the Monday shell schedule mails it)
"""
import argparse, json, os, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from store import open_store  # noqa: E402

DAY = 86400.0


def n(store, sql, *a):
    return store.one(sql, *a)["n"]


def build(store, now=None, days=7):
    now = now or time.time()
    since = now - days * DAY
    lines = {}

    revenue = n(store, "SELECT COALESCE(SUM(amount_cents),0) n FROM ledger WHERE paid_at>?", since) / 100
    refunds = n(store, "SELECT COALESCE(SUM(amount_cents),0) n FROM refunds WHERE state='issued' AND created_at>?", since) / 100
    lines["Cash"] = f"${revenue:,.2f} collected, ${refunds:,.2f} refunded in {days} days"

    paid = n(store, "SELECT COUNT(*) n FROM events WHERE name='cloud.paid' AND ts>?", since)
    live = n(store, "SELECT COUNT(*) n FROM servers WHERE status IN ('ready','provisioning')")
    churned = n(store, "SELECT COUNT(*) n FROM lifecycle_actions WHERE state='done' AND shadow=0 AND decided_at>?", since)
    lines["Customers"] = f"{paid} new, {churned} servers removed after lapsing, {live} servers live"

    ready = store.all("SELECT props FROM events WHERE name='cloud.ready' AND ts>?", since)
    secs = sorted(json.loads(r["props"]).get("ms_from_payment", 0) / 1000 for r in ready)
    lines["Provisioning"] = (f"{len(secs)} ready, median {secs[len(secs)//2]:.0f}s, slowest {secs[-1]:.0f}s" if secs
                             else "nothing provisioned this week")
    failed = n(store, "SELECT COUNT(*) n FROM events WHERE name='cloud.provision_failed' AND ts>?", since)
    if failed:
        lines["Provisioning"] += f"; {failed} failed and refunded"

    stale = store.all("SELECT slug, last_backup_at FROM servers WHERE status='ready' AND "
                      "(last_backup_at IS NULL OR last_backup_at<?)", now - 3600)
    lines["Backups"] = (f"all {live} live servers backed up within the hour" if live and not stale else
                        "no live servers" if not live else
                        f"{len(stale)} of {live} have no backup in the last hour: " + ", ".join(s['slug'] for s in stale[:10]))

    acts = {r["state"]: r["c"] for r in store.all("SELECT state, COUNT(*) c FROM lifecycle_actions WHERE proposed_at>? GROUP BY state", since)}
    shadow = store.flag("lifecycle_shadow") == "1"
    lines["Lifecycle"] = (("shadow mode: " if shadow else "") +
                          ", ".join(f"{v} {k}" for k, v in sorted(acts.items())) if acts else
                          ("shadow mode, nothing proposed" if shadow else "nothing proposed"))
    paused = n(store, "SELECT COUNT(*) n FROM events WHERE name='lifecycle.paused' AND ts>?", since)
    if paused:
        lines["Lifecycle"] += f"; paused {paused} times by the health gate"

    rel = store.one("SELECT * FROM releases ORDER BY created_at DESC LIMIT 1")
    lines["Releases"] = ("no release published" if not rel else
                         f"{rel['version']} at stage {rel['stage']}" + (", HALTED" if rel["halted"] else "") +
                         f", {rel['rollbacks']} rollback(s)")

    syn = store.all("SELECT props FROM events WHERE name='synthetic.run' AND ts>?", since)
    if syn:
        ok = sum(1 for r in syn if json.loads(r["props"]).get("ok"))
        lines["Synthetic journey"] = f"{ok} of {len(syn)} runs passed"
    else:
        lines["Synthetic journey"] = "not measured: no synthetic runs recorded this week"

    waitlist = n(store, "SELECT COUNT(*) n FROM waitlist")
    new_wait = n(store, "SELECT COUNT(*) n FROM waitlist WHERE created_at>?", since)
    lines["Waitlist"] = f"{waitlist} total, {new_wait} this week"

    owner_min = store.all("SELECT props FROM events WHERE name='owner.minutes' AND ts>?", since)
    lines["Owner time"] = (f"{sum(json.loads(r['props']).get('minutes', 0) for r in owner_min):.0f} minutes"
                           if owner_min else "not measured: no owner actions logged")

    alerts = store.all("SELECT kind, message FROM alerts WHERE acknowledged=0 ORDER BY id DESC LIMIT 20")
    queued = store.all("SELECT org_id, amount_cents FROM refunds WHERE state='queued'")
    waiting = [f"{a['kind']}: {a['message']}" for a in alerts] + \
              [f"refund of ${q['amount_cents']/100:.2f} for {q['org_id']} over the cap (paid by default after 7 days)" for q in queued]
    return lines, waiting


def render(lines, waiting, days=7):
    out = [f"amux Cloud, the last {days} days", ""]
    for k, v in lines.items():
        out += [f"- {k}: {v}", ""]
    out += ["Decisions waiting:" if waiting else "Nothing is waiting for you.", ""]
    for w in waiting:
        out += [f"- {w}", ""]
    return "\n".join(out).rstrip() + "\n"


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=7)
    a = ap.parse_args()
    lines, waiting = build(open_store(), days=a.days)
    sys.stdout.write(render(lines, waiting, a.days))
