#!/usr/bin/env python3
"""Make a Stripe TEST-mode account ready for the control plane (docs/design/cloud-self-service.md, T-PAY-1).

Idempotent. For each plan in plans.json: a product and a monthly price carrying the plan's
stripe_lookup_key (the control plane finds prices by lookup key). Then one webhook endpoint at
<control>/webhooks/stripe for the events accounts.py handles. Prints the webhook signing secret
the first time it creates the endpoint, which Stripe shows only then.

  stripe_setup.py --control https://cp-test.amux.io                 (key from STRIPE_TEST_SECRET_KEY)
  stripe_setup.py --control https://cloud.amux.io --live          (key from STRIPE_LIVE_SECRET_KEY)

--live is the owner's launch decision (2026-10-17: "go ahead and launch everything"); without it a
live key is refused. Also makes the promotion codes: FIRSTMONTH (a customer's first month free,
owner 2026-10-17) and, with --synth-code, the synthetic customer's code. Both use one 100%-off,
first-invoice-only coupon limited to the amux Cloud products, because the same Stripe account
sells Mixpeek.
"""
import argparse, json, os, sys, urllib.error, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
EVENTS = ["checkout.session.completed", "customer.subscription.created", "customer.subscription.updated",
          "customer.subscription.deleted", "invoice.paid", "invoice.payment_failed", "charge.dispute.created"]


def form(d, prefix=""):
    out = []
    for k, v in d.items():
        key = f"{prefix}[{k}]" if prefix else k
        if isinstance(v, dict):
            out += form(v, key)
        elif isinstance(v, list):
            out += [(f"{key}[]", str(x)) for x in v]
        else:
            out.append((key, str(v)))
    return out


def call(key, method, path, params=None):
    data = urllib.parse.urlencode(form(params or {})).encode() if method == "POST" else None
    url = "https://api.stripe.com/v1" + path + ("?" + urllib.parse.urlencode(form(params or {})) if method == "GET" and params else "")
    req = urllib.request.Request(url, data=data, method=method, headers={"Authorization": f"Bearer {key}"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read())


COUPON = "amux_cloud_first_month_free"
DESCRIPTOR = "AMUX CLOUD"   # the card statement shows amux, not the account's Mixpeek default


def promo_code(key, code, coupon, first_time):
    if call(key, "GET", "/promotion_codes", {"code": code, "limit": 1})["data"]:
        print(f"promotion code {code}: present", file=sys.stderr)
        return
    params = {"code": code}
    if first_time:
        params["restrictions"] = {"first_time_transaction": "true"}
    try:
        call(key, "POST", "/promotion_codes", dict(params, coupon=coupon))
    except urllib.error.HTTPError as e:      # newer API versions take promotion[coupon]
        if e.code != 400:
            raise
        call(key, "POST", "/promotion_codes", dict(params, promotion={"type": "coupon", "coupon": coupon}))
    print(f"promotion code {code}: created", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--control", required=True, help="control plane URL, e.g. https://cp-test.amux.io")
    ap.add_argument("--live", action="store_true", help="create live objects with STRIPE_LIVE_SECRET_KEY")
    ap.add_argument("--promo", default="FIRSTMONTH", help="customer code for a free first month")
    ap.add_argument("--portal-out", default="", help="write the amux billing portal configuration id here")
    ap.add_argument("--synth-code", default="", help="the synthetic customer's code (live mode pays with it)")
    a = ap.parse_args()
    if a.live:
        key = os.environ.get("STRIPE_LIVE_SECRET_KEY", "")
        if not key.startswith(("sk_live_", "rk_live_")):
            sys.exit("--live needs STRIPE_LIVE_SECRET_KEY=sk_live_...")
    else:
        key = os.environ.get("STRIPE_TEST_SECRET_KEY", "")
        if not key.startswith(("sk_test_", "rk_test_")):
            sys.exit("STRIPE_TEST_SECRET_KEY must be a test-mode key (sk_test_...); pass --live for live objects")
    plans = json.load(open(os.path.join(HERE, "plans.json")))
    products = []
    for p in plans["plans"]:
        lk = p["stripe_lookup_key"]
        found = call(key, "GET", "/prices", {"lookup_keys": [lk], "active": "true"})["data"]
        if found:
            products.append(found[0]["product"])
            call(key, "POST", f"/products/{found[0]['product']}", {"statement_descriptor": DESCRIPTOR})
            print(f"price {lk}: present", file=sys.stderr)
            continue
        prod = call(key, "POST", "/products", {"name": f"amux Cloud {p['name']}", "description": p.get("specs", ""),
                                                "metadata": {"plan": p["id"]}, "statement_descriptor": DESCRIPTOR})
        call(key, "POST", "/prices", {"product": prod["id"], "currency": plans.get("currency", "usd"),
                                      "unit_amount": int(round(p["price_usd"] * 100)), "recurring": {"interval": "month"},
                                      "lookup_key": lk, "tax_behavior": "exclusive"})
        products.append(prod["id"])
        print(f"price {lk}: created at ${p['price_usd']}/month", file=sys.stderr)
    try:
        call(key, "GET", f"/coupons/{COUPON}")
        print(f"coupon {COUPON}: present", file=sys.stderr)
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        call(key, "POST", "/coupons", {"id": COUPON, "name": "First month free", "percent_off": 100,
                                       "duration": "once", "applies_to": {"products": products}})
        print(f"coupon {COUPON}: created (100% off the first invoice, amux Cloud products only)", file=sys.stderr)
    promo_code(key, a.promo, COUPON, first_time=True)
    if a.synth_code:
        promo_code(key, a.synth_code, COUPON, first_time=False)
    if a.portal_out:
        # The account's default portal is Mixpeek's. amux sessions name this one instead: amux
        # headline and legal links, card and invoice changes, cancel at period end, no plan
        # switching (a plan change would need a server resize, which is not built).
        conf = {"business_profile": {"headline": "amux Cloud billing", "privacy_policy_url": "https://amux.io/privacy/",
                                     "terms_of_service_url": "https://amux.io/terms/"},
                "features": {"invoice_history": {"enabled": True}, "payment_method_update": {"enabled": True},
                             "customer_update": {"enabled": True, "allowed_updates": ["email", "address", "tax_id"]},
                             "subscription_cancel": {"enabled": True, "mode": "at_period_end"},
                             "subscription_update": {"enabled": False}},
                "default_return_url": "https://amux.io/start/billing/", "metadata": {"amux_cloud": "1"}}
        got = [c for c in call(key, "GET", "/billing_portal/configurations", {"limit": 100, "active": "true"})["data"]
               if (c.get("metadata") or {}).get("amux_cloud") == "1"]
        c = call(key, "POST", f"/billing_portal/configurations/{got[0]['id']}" if got else "/billing_portal/configurations", conf)
        print(f"billing portal {c['id']}: {'updated' if got else 'created'}", file=sys.stderr)
        with open(a.portal_out, "w") as f:
            f.write(c["id"] + "\n")
    url = a.control.rstrip("/") + "/webhooks/stripe"
    hooks = call(key, "GET", "/webhook_endpoints", {"limit": 100})["data"]
    if any(h["url"] == url for h in hooks):
        print(f"webhook {url}: present (its secret is only shown at creation)", file=sys.stderr)
        return 0
    h = call(key, "POST", "/webhook_endpoints", {"url": url, "enabled_events": EVENTS,
                                                  "description": "amux Cloud control plane"})
    print(f"webhook {url}: created", file=sys.stderr)
    print(h["secret"])          # stdout: the signing secret, for the caller to store
    return 0


if __name__ == "__main__":
    sys.exit(main())
