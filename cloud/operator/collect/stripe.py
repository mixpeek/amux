#!/usr/bin/env python3
"""Stripe with a restricted read-only key: active subscriptions by plan and the last 30 days of charges."""
import time, urllib.parse
import _common as c

key = c.env("STRIPE_RESTRICTED_READ_KEY")


def collect():
    h = {"Authorization": f"Bearer {key}"}
    subs = c.fetch("https://api.stripe.com/v1/subscriptions?status=active&limit=100", h)["data"]
    since = int(time.time()) - 30 * 86400
    charges = c.fetch("https://api.stripe.com/v1/charges?" + urllib.parse.urlencode({"limit": 100, "created[gte]": since}), h)["data"]
    by_plan = {}
    for s in subs:
        plan = (s.get("metadata") or {}).get("plan", "unknown")
        by_plan[plan] = by_plan.get(plan, 0) + 1
    return {"active_subscriptions": len(subs), "by_plan": by_plan, "has_more": len(subs) == 100,
            "charges_30d": sum(ch["amount"] for ch in charges if ch["paid"] and not ch["refunded"]) / 100,
            "refunds_30d": sum(ch["amount_refunded"] for ch in charges) / 100,
            "disputes_30d": sum(1 for ch in charges if ch.get("disputed"))}


if not key:
    c.write("stripe", why_unmeasured="STRIPE_RESTRICTED_READ_KEY unset")
else:
    c.run("stripe", collect)
