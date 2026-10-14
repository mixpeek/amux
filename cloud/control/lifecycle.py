"""Safe lifecycle for customer servers (docs/design/cloud-self-service.md, section 3.2).

Nothing here deletes on one signal. A webhook or the hourly sweep can only
PROPOSE. Execution needs, in order:

  1. every provider healthy (payment provider API, webhook flow, Hetzner, backups)
  2. the hold period over (14 days after a payment failure; period end on cancel)
  3. two sources agreeing: a FRESH read of the subscription from the provider's
     API, and our own ledger showing no payment since the failure or period end
  4. the run under the breaker: at most max(3, 5% of the fleet) deletions per
     run, else lifecycle freezes and the owner is alerted; at most 3 per day
  5. a snapshot the Hetzner API lists with a real size, and a backup under 1 hour old

Shadow mode (on by default) records every decision and executes none. Each
shadow decision is replayed against the provider 14 days later; execution turns
on by itself after 30 days and 5 agreeing replays with no disagreement.

Servers without our label are never touched; labelled servers without a row are
reported, never deleted.
"""
import json, math, time

from store import new_id

DAY = 86400.0
PAYMENT_FAILURE_GRACE = 14 * DAY
SNAPSHOT_RETENTION = 30 * DAY
REPLAY_AFTER = 14 * DAY
SHADOW_MIN_AGE = 30 * DAY
SHADOW_MIN_REPLAYS = 5
DAILY_DELETE_CAP = 3
BACKUP_MAX_AGE = 3600.0
WEBHOOK_SILENCE = 6 * 3600.0
LABEL = "amux-cloud"
MIN_WARNING = 3 * DAY

ENDED = {"canceled", "unpaid", "incomplete_expired"}
FAILING = {"past_due", "unpaid"}


class ProviderError(Exception):
    """A provider could not be read. Any destructive step that needed it pauses."""


# ---------------------------------------------------------------- decisions
def wanted_action(sub, now):
    """What the subscription row (webhook-fed) suggests. Returns (reason, hold_until) or None."""
    if sub is None:
        return None
    if sub["status"] == "canceled" and (sub["current_period_end"] or 0) <= now:
        return ("cancelled", sub["current_period_end"] or now)
    if sub["status"] in FAILING and sub["failure_started_at"]:
        return ("payment_failed", sub["failure_started_at"] + PAYMENT_FAILURE_GRACE)
    return None


def sources_agree(store, billing, action, server, now):
    """Rule 3. Returns (agree: bool, evidence: dict). Raises ProviderError if the API cannot be read."""
    fresh = billing.get_subscription(server["subscription_id"])          # may raise ProviderError
    sub = store.one("SELECT * FROM subscriptions WHERE id=?", server["subscription_id"]) or {}
    since = (sub.get("failure_started_at") if action["reason"] == "payment_failed"
             else sub.get("current_period_end")) or 0
    paid_since = store.one("SELECT COUNT(*) n FROM ledger WHERE subscription_id=? AND paid_at>?",
                           server["subscription_id"], since)["n"]
    if action["reason"] == "cancelled":
        api_says_ended = fresh["status"] in ENDED and (fresh.get("current_period_end") or 0) <= now
    else:
        api_says_ended = fresh["status"] in ENDED or (
            fresh["status"] in FAILING and now >= since + PAYMENT_FAILURE_GRACE)
    evidence = {"api_status": fresh["status"], "api_period_end": fresh.get("current_period_end"),
                "ledger_payments_since": paid_since, "since": since, "checked_at": now}
    return (api_says_ended and paid_since == 0), evidence


def unhealthy_reasons(store, billing, infra, now):
    """Rule 4 of the plan (health gate). An empty list means healthy."""
    reasons = []
    for name, probe in (("payment provider", billing.healthy), ("hetzner", infra.healthy)):
        try:
            if not probe():
                reasons.append(f"{name} unhealthy")
        except Exception as e:  # a probe that throws is as bad as one that says no
            reasons.append(f"{name} probe failed: {e}")
    last_checkout = store.one("SELECT MAX(ts) t FROM events WHERE name='cloud.checkout_started'")["t"]
    last_webhook = store.one("SELECT MAX(ts) t FROM events WHERE name='stripe.webhook'")["t"]
    if last_checkout and now - last_checkout < WEBHOOK_SILENCE and (
            not last_webhook or now - last_webhook > WEBHOOK_SILENCE):
        reasons.append("webhooks silent for 6 hours while checkouts happened")
    return reasons


def breaker_limit(fleet_size):
    return max(3, math.floor(0.05 * fleet_size))


# ---------------------------------------------------------------- sweep
def propose(store, server, reason, hold_until, now, shadow):
    states = "('proposed','held','executing','shadowed')" if shadow else "('proposed','held','executing')"
    open_ = store.one("SELECT id FROM lifecycle_actions WHERE server_id=? AND kind='delete_server' "
                      f"AND state IN {states}", server["id"])
    if open_:
        return open_["id"]
    if not shadow:
        hold_until = max(hold_until, now + MIN_WARNING)   # every customer is warned at least 3 days ahead
        # Spread due dates so at most DAILY_DELETE_CAP fall in any 24 hours.
        while store.one("SELECT COUNT(*) n FROM lifecycle_actions WHERE shadow=0 AND state IN ('held','proposed') "
                        "AND hold_until>? AND hold_until<=?", hold_until - DAY, hold_until + DAY)["n"] >= DAILY_DELETE_CAP:
            hold_until += DAY
    aid = new_id("lc")
    store.run("INSERT INTO lifecycle_actions(id,server_id,kind,reason,state,shadow,proposed_at,hold_until) "
              "VALUES(?,?,?,?,?,?,?,?)", aid, server["id"], "delete_server", reason, "held",
              1 if shadow else 0, now, hold_until)
    store.event("lifecycle.proposed", server["org_id"], action=aid, reason=reason, hold_until=hold_until,
                shadow=bool(shadow))
    org = store.one("SELECT email FROM orgs WHERE id=?", server["org_id"])
    if org and not shadow:
        store.enqueue_email(org["email"], "server_will_be_snapshotted", dedupe=f"warn:{aid}",
                            slug=server["slug"], when=hold_until)
    return aid


def sweep(store, billing, infra, now=None):
    """The hourly reconcile (M5). Returns a summary dict that the weekly report reads."""
    now = now or time.time()
    shadow = store.flag("lifecycle_shadow") == "1"
    summary = {"now": now, "shadow": shadow, "proposed": 0, "cancelled": 0, "executed": 0,
               "shadowed": 0, "paused": [], "breaker": False, "orphans": [], "replayed": 0}

    live = store.all("SELECT * FROM servers WHERE status IN ('ready','provisioning')")
    fleet = len(live)
    # Anomaly breaker at proposal time: many lapses appearing in one sweep means bad data or a
    # provider fault, not churn. Customers re-proposed after shadow mode ended do not count.
    candidates = []
    for srv in live:
        sub = store.one("SELECT * FROM subscriptions WHERE id=?", srv["subscription_id"]) if srv["subscription_id"] else None
        want = wanted_action(sub, now)
        if want:
            candidates.append((srv, want))
    fresh = [c for c in candidates if not store.one(
        "SELECT 1 x FROM lifecycle_actions WHERE server_id=? AND kind='delete_server' AND state IN "
        "('proposed','held','executing','shadowed','done')", c[0]["id"])]
    if not shadow and len(fresh) > breaker_limit(fleet) and store.flag("lifecycle_freeze") != "1":
        store.set_flag("lifecycle_freeze", "1")
        store.alert("lifecycle_breaker", f"{len(fresh)} servers lapsed in one sweep with a fleet of {fleet} "
                    f"(limit {breaker_limit(fleet)}); lifecycle frozen, nothing proposed or deleted")
        summary["breaker"] = True
        return _done(store, summary)
    # Proposals and cancellations come from our own rows; they never delete anything.
    for srv in live:
        sub = store.one("SELECT * FROM subscriptions WHERE id=?", srv["subscription_id"]) if srv["subscription_id"] else None
        want = wanted_action(sub, now)
        if want:
            propose(store, srv, want[0], want[1], now, shadow)
            summary["proposed"] += 1
        else:
            for a in store.all("SELECT id FROM lifecycle_actions WHERE server_id=? AND state IN ('proposed','held')", srv["id"]):
                store.run("UPDATE lifecycle_actions SET state='cancelled', decided_at=? WHERE id=?", now, a["id"])
                store.event("lifecycle.cancelled", srv["org_id"], action=a["id"], why="subscription active again")
                summary["cancelled"] += 1

    summary["orphans"] = report_orphans(store, infra)
    summary["replayed"] = replay_shadow(store, billing, now)
    maybe_leave_shadow(store, now)

    if store.flag("lifecycle_freeze") == "1":
        summary["paused"].append("lifecycle_freeze is on")
        return _done(store, summary)
    reasons = unhealthy_reasons(store, billing, infra, now)
    if reasons:
        summary["paused"] = reasons
        store.event("lifecycle.paused", reasons=reasons)
        return _done(store, summary)

    due = store.all("SELECT a.*, s.org_id, s.subscription_id, s.slug, s.hetzner_id, s.dns_record_id, "
                    "s.last_backup_at FROM lifecycle_actions a JOIN servers s ON s.id=a.server_id "
                    "WHERE a.state='held' AND a.kind='delete_server' AND a.hold_until<=? ORDER BY a.hold_until", now)
    confirmed = []
    for a in due:
        srv = {"id": a["server_id"], "org_id": a["org_id"], "subscription_id": a["subscription_id"]}
        try:
            agree, ev = sources_agree(store, billing, a, srv, now)
        except ProviderError as e:
            summary["paused"].append(f"payment provider read failed: {e}")
            store.event("lifecycle.paused", reasons=[str(e)])
            return _done(store, summary)
        store.run("UPDATE lifecycle_actions SET evidence=? WHERE id=?", json.dumps(ev, sort_keys=True), a["id"])
        if not agree:
            store.run("UPDATE lifecycle_actions SET state='cancelled', decided_at=? WHERE id=?", now, a["id"])
            store.alert("lifecycle_disagreement",
                        f"{a['slug']}: our records said {a['reason']} but {json.dumps(ev, sort_keys=True)}; nothing deleted")
            summary["cancelled"] += 1
            continue
        confirmed.append(a)

    if shadow and len(confirmed) > breaker_limit(fleet):
        # Nothing is deleted in shadow mode, so record what the breaker would have done and carry on.
        store.event("lifecycle.shadow_breaker", due=len(confirmed), fleet=fleet, limit=breaker_limit(fleet))
    elif len(confirmed) > breaker_limit(fleet):
        store.set_flag("lifecycle_freeze", "1")
        store.alert("lifecycle_breaker", f"{len(confirmed)} deletions due in one run with a fleet of {fleet} "
                    f"(limit {breaker_limit(fleet)}); lifecycle frozen, nothing deleted")
        summary["breaker"] = True
        return _done(store, summary)

    deleted_today = store.one("SELECT COUNT(*) n FROM lifecycle_actions WHERE state='done' AND shadow=0 "
                              "AND decided_at>?", now - DAY)["n"]
    for a in confirmed:
        if a["shadow"] and not shadow:
            # Proposed while shadowing, so the customer was never warned: warn now, wait 3 days.
            store.run("UPDATE lifecycle_actions SET shadow=0, state='held', hold_until=? WHERE id=?", now + 3 * DAY, a["id"])
            org = store.one("SELECT email FROM orgs WHERE id=?", a["org_id"])
            if org:
                store.enqueue_email(org["email"], "server_will_be_snapshotted", dedupe=f"warn:{a['id']}",
                                    slug=a["slug"], when=now + 3 * DAY)
            continue
        if shadow:
            store.run("UPDATE lifecycle_actions SET state='shadowed', shadow=1, decided_at=? WHERE id=?", now, a["id"])
            store.event("lifecycle.shadowed", a["org_id"], action=a["id"], slug=a["slug"])
            summary["shadowed"] += 1
            continue
        if deleted_today >= DAILY_DELETE_CAP:
            store.event("lifecycle.daily_cap", a["org_id"], action=a["id"])
            continue
        if execute_delete(store, infra, a, now):
            deleted_today += 1
            summary["executed"] += 1
    return _done(store, summary)


def execute_delete(store, infra, a, now):
    """Rule 6: snapshot, verify it, verify the backup, then delete. Any doubt leaves the server running."""
    if not a["last_backup_at"] or now - a["last_backup_at"] > BACKUP_MAX_AGE:
        store.event("lifecycle.blocked", a["org_id"], action=a["id"], why="no backup in the last hour")
        return False
    store.run("UPDATE lifecycle_actions SET state='executing' WHERE id=?", a["id"])
    try:
        image_id = a["snapshot_id"] or infra.create_snapshot(a["hetzner_id"], f"amux-cloud {a['slug']} before deletion")
        store.run("UPDATE lifecycle_actions SET snapshot_id=? WHERE id=?", str(image_id), a["id"])
        img = infra.get_image(image_id)
        if not img or img.get("status") != "available" or not (img.get("image_size") or 0) > 0:
            store.run("UPDATE lifecycle_actions SET state='held' WHERE id=?", a["id"])
            store.event("lifecycle.blocked", a["org_id"], action=a["id"], why=f"snapshot not verified: {img}")
            return False
        infra.delete_server(a["hetzner_id"])
        if a["dns_record_id"]:
            infra.delete_dns(a["dns_record_id"])
    except Exception as e:
        store.run("UPDATE lifecycle_actions SET state='held' WHERE id=?", a["id"])
        store.event("lifecycle.blocked", a["org_id"], action=a["id"], why=f"provider error: {e}")
        return False
    store.run("UPDATE lifecycle_actions SET state='done', decided_at=? WHERE id=?", now, a["id"])
    store.run("UPDATE servers SET status='deleted' WHERE id=?", a["server_id"])
    store.event("lifecycle.deleted", a["org_id"], action=a["id"], slug=a["slug"], snapshot=str(image_id))
    org = store.one("SELECT email FROM orgs WHERE id=?", a["org_id"])
    if org:
        store.enqueue_email(org["email"], "server_snapshotted", dedupe=f"done:{a['id']}", slug=a["slug"],
                            snapshot_expires=now + SNAPSHOT_RETENTION)
    return True


def report_orphans(store, infra):
    """Rule 7: labelled servers we have no live row for are reported, never deleted."""
    try:
        labelled = infra.list_servers(label_selector=f"{LABEL}=1")
    except Exception as e:
        store.event("lifecycle.orphan_scan_failed", error=str(e))
        return []
    known = {r["hetzner_id"] for r in store.all("SELECT hetzner_id FROM servers WHERE status IN ('ready','provisioning') "
                                                "AND hetzner_id IS NOT NULL")}
    orphans = [s["id"] for s in labelled if s["id"] not in known]
    for o in orphans:
        store.event("lifecycle.orphan", hetzner_id=o)
    return orphans


def replay_shadow(store, billing, now):
    """Rule 8: 14 days after a shadow decision, check it against a fresh provider read."""
    n = 0
    rows = store.all("SELECT a.*, s.org_id, s.subscription_id, s.slug FROM lifecycle_actions a "
                     "JOIN servers s ON s.id=a.server_id WHERE a.state='shadowed' AND a.replay_at IS NULL "
                     "AND a.decided_at<=?", now - REPLAY_AFTER)
    for a in rows:
        srv = {"id": a["server_id"], "org_id": a["org_id"], "subscription_id": a["subscription_id"]}
        try:
            agree, ev = sources_agree(store, billing, a, srv, now)
        except ProviderError:
            continue
        verdict = "agree" if agree else "disagree"
        store.run("UPDATE lifecycle_actions SET replay_at=?, replay_verdict=? WHERE id=?", now, verdict, a["id"])
        store.event("lifecycle.replay", a["org_id"], action=a["id"], verdict=verdict, evidence=ev)
        if not agree:
            store.alert("lifecycle_shadow_disagreement",
                        f"{a['slug']}: a shadow deletion would have been wrong ({json.dumps(ev, sort_keys=True)}); "
                        "shadow mode stays on")
        n += 1
    return n


def maybe_leave_shadow(store, now):
    if store.flag("lifecycle_shadow") != "1":
        return False
    first = store.one("SELECT MIN(decided_at) t FROM lifecycle_actions WHERE shadow=1 AND state='shadowed'")["t"]
    agree = store.one("SELECT COUNT(*) n FROM lifecycle_actions WHERE replay_verdict='agree'")["n"]
    disagree = store.one("SELECT COUNT(*) n FROM lifecycle_actions WHERE replay_verdict='disagree'")["n"]
    if first and now - first >= SHADOW_MIN_AGE and agree >= SHADOW_MIN_REPLAYS and disagree == 0:
        store.set_flag("lifecycle_shadow", "0")
        store.event("lifecycle.shadow_ended", replays=agree)
        return True
    return False


def _done(store, summary):
    store.event("lifecycle.sweep", **{k: v for k, v in summary.items() if k != "now"})
    return summary
