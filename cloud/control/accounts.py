"""Checkout-first accounts, Stripe events, refunds and disputes
(docs/design/cloud-self-service.md, D6, J4-J5, J16-J17, M1, M20).

complete_checkout() is called by both the success URL and the
checkout.session.completed webhook. Whichever arrives first does the work; the
other finds the signup row and returns the same result.
"""
import sqlite3, time

import provision
from store import new_id

DAY = 86400.0
REFUND_CAP_FRACTION, REFUND_CAP_USD = 0.02, 500.0
DISPUTE_ACCEPT_BELOW_CENTS = 1000


class NotPaid(Exception):
    pass


def _cust_id(c):
    return c["id"] if isinstance(c, dict) else c


def complete_checkout(store, billing, session_id, now=None):
    now = now or time.time()
    done = store.one("SELECT * FROM signups WHERE checkout_session_id=?", session_id)
    if done and done["server_id"]:
        return _result(store, done)
    sess = billing.get_checkout_session(session_id)
    if sess.get("status") != "complete" or sess.get("payment_status") not in ("paid", "no_payment_required"):
        raise NotPaid(f"checkout {session_id} is {sess.get('status')}/{sess.get('payment_status')}")
    meta = sess.get("metadata") or {}
    plan = meta.get("plan") or "solo"
    email = ((sess.get("customer_details") or {}).get("email") or sess.get("customer_email") or "").strip().lower()
    customer = _cust_id(sess.get("customer"))
    sub_obj = sess.get("subscription")
    sub_id = _cust_id(sub_obj)
    if not email or not sub_id:
        raise NotPaid(f"checkout {session_id} has no email or subscription")

    # The signup row is the lock, taken before anything is created: the loser of a
    # success-URL/webhook race waits for the winner's result and returns it.
    try:
        store.run("INSERT INTO signups(checkout_session_id,org_id,plan,created_at) VALUES(?,?,?,?)",
                  session_id, "pending", plan, now)
    except sqlite3.IntegrityError:
        for _ in range(100):
            row = store.one("SELECT * FROM signups WHERE checkout_session_id=?", session_id)
            if row["server_id"]:
                return _result(store, row)
            time.sleep(0.1)
        raise TimeoutError(f"checkout {session_id} is being completed by another request")
    try:
        return _complete(store, billing, session_id, sess, meta, plan, email, customer, sub_obj, sub_id, now)
    except Exception:
        store.run("DELETE FROM signups WHERE checkout_session_id=? AND server_id IS NULL", session_id)   # let a retry run
        raise


def _complete(store, billing, session_id, sess, meta, plan, email, customer, sub_obj, sub_id, now):
    # Existing account by Stripe customer, then by email (T-ACC-2); else a new one (T-ACC-1).
    org = (store.one("SELECT * FROM orgs WHERE stripe_customer_id=?", customer) if customer else None) or \
          store.one("SELECT o.* FROM orgs o JOIN users u ON u.org_id=o.id WHERE lower(u.email)=?", email)
    if not org:
        oid = new_id("org")
        store.run("INSERT INTO orgs(id,email,stripe_customer_id,source,created_at) VALUES(?,?,?,?,?)",
                  oid, email, customer, meta.get("source"), now)
        org = store.one("SELECT * FROM orgs WHERE id=?", oid)
    elif customer and not org["stripe_customer_id"]:
        store.run("UPDATE orgs SET stripe_customer_id=? WHERE id=?", customer, org["id"])
    user = store.one("SELECT * FROM users WHERE lower(email)=?", email)
    if not user:
        uid = new_id("usr")
        store.run("INSERT INTO users(id,email,email_verified,org_id,created_at) VALUES(?,?,0,?,?)", uid, email, org["id"], now)
        store.run("UPDATE orgs SET owner_user_id=? WHERE id=? AND owner_user_id IS NULL", uid, org["id"])
        user = store.one("SELECT * FROM users WHERE id=?", uid)

    sub = billing.get_subscription(sub_id) if not isinstance(sub_obj, dict) else _norm(sub_obj)
    upsert_subscription(store, sub, org["id"], plan, now)
    if sess.get("amount_total"):
        inv = (sub_obj.get("latest_invoice") if isinstance(sub_obj, dict) else None) or f"checkout:{session_id}"
        store.run("INSERT OR IGNORE INTO ledger(invoice_id,org_id,subscription_id,amount_cents,paid_at,payment_intent) "
                  "VALUES(?,?,?,?,?,?)", _cust_id(inv), org["id"], sub_id, sess["amount_total"], now,
                  sess.get("payment_intent"))
    srv = provision.start(store, org["id"], sub_id, plan, email, now=now)
    store.run("UPDATE signups SET server_id=?, org_id=? WHERE checkout_session_id=?", srv["id"], org["id"], session_id)
    store.event("cloud.paid", org["id"], plan=plan, amount=sess.get("amount_total"), source=meta.get("source"))
    store.enqueue_email(email, "being_built", dedupe=f"built:{srv['id']}", slug=srv["slug"])
    return _result(store, store.one("SELECT * FROM signups WHERE checkout_session_id=?", session_id))


def _norm(s):
    from billing import normalize_subscription
    return normalize_subscription(s)


def _result(store, signup):
    org = store.one("SELECT * FROM orgs WHERE id=?", signup["org_id"])
    return {"org": org, "user": store.one("SELECT * FROM users WHERE id=?", org["owner_user_id"]),
            "server": store.one("SELECT * FROM servers WHERE id=?", signup["server_id"])}


def upsert_subscription(store, sub, org_id, plan, now):
    store.run("INSERT INTO subscriptions(id,org_id,plan,status,current_period_end,cancel_at_period_end,updated_at) "
              "VALUES(?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET status=excluded.status, "
              "current_period_end=excluded.current_period_end, cancel_at_period_end=excluded.cancel_at_period_end, "
              "updated_at=excluded.updated_at", sub["id"], org_id, sub.get("plan") or plan, sub["status"],
              sub.get("current_period_end"), int(bool(sub.get("cancel_at_period_end"))), now)


# ---------------------------------------------------------------- webhooks
def handle_event(store, billing, event, now=None, on_plan_change=None):
    """Stripe webhook dispatcher. Each event id is processed once.
    on_plan_change(sub_id, new_plan) is called when the subscription's price moved to another plan (J15)."""
    now = now or time.time()
    store.event("stripe.webhook", type=event.get("type"))
    try:
        store.run("INSERT INTO stripe_events(id,type,ts) VALUES(?,?,?)", event["id"], event["type"], now)
    except sqlite3.IntegrityError:
        return "duplicate"
    t, obj = event["type"], event["data"]["object"]
    if t == "checkout.session.completed":
        if obj.get("mode") == "subscription":
            complete_checkout(store, billing, obj["id"], now)
        return "checkout"
    if t in ("customer.subscription.updated", "customer.subscription.deleted", "customer.subscription.created"):
        sub = _norm(obj)
        row = store.one("SELECT org_id, plan FROM subscriptions WHERE id=?", sub["id"])
        if row:
            new_plan = plan_for_lookup(sub.get("price_lookup_key")) or row["plan"]
            upsert_subscription(store, dict(sub, plan=new_plan), row["org_id"], row["plan"], now)
            store.run("UPDATE subscriptions SET plan=? WHERE id=?", new_plan, sub["id"])
            if sub["status"] == "active":
                store.run("UPDATE subscriptions SET failure_started_at=NULL WHERE id=?", sub["id"])
            if new_plan != row["plan"] and on_plan_change:
                on_plan_change(sub["id"], new_plan)
        return "subscription"
    if t == "invoice.paid":
        sub_id = _cust_id(obj.get("subscription") or ((obj.get("parent") or {}).get("subscription_details") or {}).get("subscription"))
        row = store.one("SELECT org_id FROM subscriptions WHERE id=?", sub_id) if sub_id else None
        if row:
            store.run("INSERT OR IGNORE INTO ledger(invoice_id,org_id,subscription_id,amount_cents,paid_at,payment_intent) "
                      "VALUES(?,?,?,?,?,?)", obj["id"], row["org_id"], sub_id, obj.get("amount_paid") or 0, now,
                      _cust_id(obj.get("payment_intent")))
            store.run("UPDATE subscriptions SET failure_started_at=NULL WHERE id=?", sub_id)
        return "paid"
    if t == "invoice.payment_failed":
        sub_id = _cust_id(obj.get("subscription") or ((obj.get("parent") or {}).get("subscription_details") or {}).get("subscription"))
        row = store.one("SELECT * FROM subscriptions WHERE id=?", sub_id) if sub_id else None
        if row:
            store.run("UPDATE subscriptions SET status='past_due', failure_started_at=COALESCE(failure_started_at, ?), "
                      "updated_at=? WHERE id=?", now, now, sub_id)
            org = store.one("SELECT email FROM orgs WHERE id=?", row["org_id"])
            store.enqueue_email(org["email"], "payment_failed", dedupe=f"payfail:{sub_id}:{int(now // DAY)}",
                                url=obj.get("hosted_invoice_url"))
        return "payment_failed"
    if t == "charge.dispute.created":
        return answer_dispute(store, billing, obj, now)
    return "ignored"


def plan_for_lookup(lookup_key):
    if not lookup_key:
        return None
    from price_check import load_plans
    return next((p["id"] for p in load_plans()["plans"] if p["stripe_lookup_key"] == lookup_key), None)


# ---------------------------------------------------------------- refunds (budget 4.7)
def refund_room_cents(store, now=None):
    now = now or time.time()
    revenue = store.one("SELECT COALESCE(SUM(amount_cents),0) c FROM ledger WHERE paid_at>?", now - 30 * DAY)["c"]
    cap = min(REFUND_CAP_FRACTION * revenue, REFUND_CAP_USD * 100)
    used = store.one("SELECT COALESCE(SUM(amount_cents),0) c FROM refunds WHERE state='issued' AND created_at>?",
                     now - 30 * DAY)["c"]
    return max(0, int(cap - used))


def refund_latest(store, billing, org_id, reason, now=None):
    """Refund the org's latest payment if it fits the cap; otherwise queue it for the owner
    (paid by default after 7 days, section 9). Returns True when issued now."""
    now = now or time.time()
    pay = store.one("SELECT * FROM ledger WHERE org_id=? AND payment_intent IS NOT NULL ORDER BY paid_at DESC LIMIT 1", org_id)
    if not pay:
        return False
    rid = new_id("rf")
    if pay["amount_cents"] <= refund_room_cents(store, now):
        billing.refund(pay["payment_intent"], pay["amount_cents"], reason)
        store.run("INSERT INTO refunds(id,org_id,payment_intent,amount_cents,reason,state,created_at,decided_at) "
                  "VALUES(?,?,?,?,?,?,?,?)", rid, org_id, pay["payment_intent"], pay["amount_cents"], reason, "issued", now, now)
        store.event("cloud.refund", org_id, amount=pay["amount_cents"], reason=reason)
        return True
    store.run("INSERT INTO refunds(id,org_id,payment_intent,amount_cents,reason,state,created_at) VALUES(?,?,?,?,?,?,?)",
              rid, org_id, pay["payment_intent"], pay["amount_cents"], reason, "queued", now)
    store.alert("refund_over_cap", f"refund of {pay['amount_cents']} cents for {org_id} ({reason}) is over the cap; "
                "it will be paid automatically in 7 days unless declined")
    return False


def settle_queued_refunds(store, billing, now=None):
    """Section 9 default: a refund over the cap that nobody declined is paid after 7 days."""
    now = now or time.time()
    for r in store.all("SELECT * FROM refunds WHERE state='queued' AND created_at<=?", now - 7 * DAY):
        billing.refund(r["payment_intent"], r["amount_cents"], r["reason"] + " (default after 7 days)")
        store.run("UPDATE refunds SET state='issued', decided_at=? WHERE id=?", now, r["id"])
        store.event("cloud.refund", r["org_id"], amount=r["amount_cents"], reason="default after 7 days")


def provisioning_refund(billing):
    def _refund(store, srv):
        return refund_latest(store, billing, srv["org_id"], "provisioning failed")
    return _refund


# ---------------------------------------------------------------- disputes (M20)
def answer_dispute(store, billing, dispute, now):
    amount = dispute.get("amount") or 0
    org = store.one("SELECT o.* FROM orgs o JOIN ledger l ON l.org_id=o.id WHERE l.payment_intent=?",
                    _cust_id(dispute.get("payment_intent")))
    if amount < DISPUTE_ACCEPT_BELOW_CENTS or not org:
        billing.close_dispute(dispute["id"])
        store.event("cloud.dispute", org["id"] if org else None, action="accepted", amount=amount)
        return "dispute_accepted"
    srv = store.one("SELECT * FROM servers WHERE org_id=? ORDER BY created_at DESC LIMIT 1", org["id"]) or {}
    active_weeks = store.one("SELECT COUNT(*) n FROM events WHERE org_id=? AND name='cloud.active_week'", org["id"])["n"]
    evidence = {
        "customer_email_address": org["email"],
        "product_description": "amux Cloud: a dedicated server running amux, billed monthly.",
        "service_date": time.strftime("%Y-%m-%d", time.gmtime(srv.get("ready_at") or org["created_at"])),
        "uncategorized_text": (f"Server {srv.get('slug')} was provisioned on "
                               f"{time.strftime('%Y-%m-%d %H:%M UTC', time.gmtime(srv.get('ready_at') or org['created_at']))} "
                               f"and reported activity in {active_weeks} week(s). The customer accepted the terms at checkout."),
    }
    billing.submit_dispute_evidence(dispute["id"], evidence)
    store.event("cloud.dispute", org["id"], action="contested", amount=amount)
    return "dispute_contested"
