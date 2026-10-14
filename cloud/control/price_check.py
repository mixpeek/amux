#!/usr/bin/env python3
"""Price guard for amux Cloud (docs/design/cloud-self-service.md, M13 and T-SITE-2).

Every price is read from cloud/control/plans.json. This script checks the three
places that must agree with it, and exits non-zero naming each disagreement:

  hetzner  every plan's price is at least guard_multiple x its server cost
           (server + IPv4 + backups), from the live Hetzner pricing API
  site     every plan price shown on site/pricing/index.html (data-plan-price
           attributes) equals plans.json
  stripe   every plan's Stripe price (by lookup key) equals plans.json

A check that cannot run (no token, no network) prints "not run" with the reason
and makes the exit status 2, so a skipped check never reads as a pass.

Usage: price_check.py [hetzner] [site] [stripe]     (default: all three)
Env:   HCLOUD_TOKEN, STRIPE_SECRET_KEY
"""
import json, os, re, sys, urllib.request, urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
PLANS_PATH = os.path.join(HERE, "plans.json")


def load_plans(path=PLANS_PATH):
    with open(path) as f:
        return json.load(f)


def monthly_cost(server_type_price_usd, plans):
    """Server price plus IPv4 plus the backup surcharge, as in the plan's 4.1."""
    return (server_type_price_usd * (1 + plans["backup_fraction"])
            + plans["ipv4_monthly_usd"])


def check_margin(plans, hetzner_prices):
    """hetzner_prices: {server_type: {location: monthly_usd_net}}. Returns problems."""
    problems = []
    for p in plans["plans"]:
        by_loc = hetzner_prices.get(p["server_type"])
        if not by_loc:
            problems.append(f"{p['id']}: server type {p['server_type']} not in Hetzner pricing")
            continue
        for loc in plans["locations"]:
            if loc not in by_loc:
                problems.append(f"{p['id']}: {p['server_type']} not offered in {loc}")
                continue
            cost = monthly_cost(by_loc[loc], plans)
            floor = cost * plans["guard_multiple"]
            if p["price_usd"] < floor:
                problems.append(f"{p['id']}: ${p['price_usd']} is under {plans['guard_multiple']}x "
                                f"cost ${cost:.2f} in {loc} (needs ${floor:.2f})")
    return problems


def check_site(plans, html):
    """The pricing page marks each Cloud price as data-plan-price="<id>:<usd>"."""
    shown = dict(re.findall(r'data-plan-price="([a-z]+):(\d+(?:\.\d+)?)"', html))
    problems = []
    for p in plans["plans"]:
        if p["id"] not in shown:
            problems.append(f"site: no data-plan-price for {p['id']}")
        elif float(shown[p["id"]]) != float(p["price_usd"]):
            problems.append(f"site: {p['id']} shows ${shown[p['id']]}, plans.json says ${p['price_usd']}")
    for extra in set(shown) - {p["id"] for p in plans["plans"]}:
        problems.append(f"site: data-plan-price for unknown plan {extra}")
    return problems


def check_stripe(plans, stripe_prices):
    """stripe_prices: {lookup_key: {"unit_amount": cents, "currency": str, "recurring": str|None}}."""
    problems = []
    for p in plans["plans"]:
        sp = stripe_prices.get(p["stripe_lookup_key"])
        if not sp:
            problems.append(f"stripe: no active price with lookup key {p['stripe_lookup_key']}")
            continue
        if sp["unit_amount"] != round(p["price_usd"] * 100):
            problems.append(f"stripe: {p['id']} is {sp['unit_amount']} cents, plans.json says {p['price_usd']} USD")
        if sp["currency"] != plans["currency"]:
            problems.append(f"stripe: {p['id']} currency {sp['currency']}")
        if sp["recurring"] != "month":
            problems.append(f"stripe: {p['id']} interval {sp['recurring']}")
    return problems


def fetch_hetzner(token):
    req = urllib.request.Request("https://api.hetzner.cloud/v1/pricing",
                                 headers={"Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=30) as r:
        data = json.load(r)["pricing"]
    out = {}
    for st in data["server_types"]:
        out[st["name"]] = {pr["location"]: float(pr["price_monthly"]["net"]) for pr in st["prices"]}
    return out


def fetch_stripe(key, lookup_keys):
    q = urllib.parse.urlencode([("active", "true"), ("limit", "100")] +
                               [("lookup_keys[]", k) for k in lookup_keys])
    req = urllib.request.Request(f"https://api.stripe.com/v1/prices?{q}",
                                 headers={"Authorization": f"Bearer {key}"})
    with urllib.request.urlopen(req, timeout=30) as r:
        data = json.load(r)["data"]
    return {p["lookup_key"]: {"unit_amount": p["unit_amount"], "currency": p["currency"],
                              "recurring": (p.get("recurring") or {}).get("interval")}
            for p in data if p.get("lookup_key")}


def main(argv):
    which = argv[1:] or ["hetzner", "site", "stripe"]
    plans = load_plans()
    problems, not_run = [], []
    if "hetzner" in which:
        tok = os.environ.get("HCLOUD_TOKEN")
        if not tok:
            not_run.append("hetzner: HCLOUD_TOKEN unset")
        else:
            problems += check_margin(plans, fetch_hetzner(tok))
    if "site" in which:
        with open(os.path.join(REPO, "site", "pricing", "index.html")) as f:
            html = f.read()
        if "data-plan-price=" not in html and plans["status"] != "approved":
            not_run.append("site: prices not published yet (plans.json status is "
                           f"'{plans['status']}', awaiting O1)")
        else:
            problems += check_site(plans, html)
    if "stripe" in which:
        key = os.environ.get("STRIPE_SECRET_KEY")
        if not key:
            not_run.append("stripe: STRIPE_SECRET_KEY unset")
        else:
            problems += check_stripe(plans, fetch_stripe(key, [p["stripe_lookup_key"] for p in plans["plans"]]))
    checked = [w for w in which if not any(n.startswith(w + ":") for n in not_run)]
    for p in problems:
        print(f"FAIL {p}")
    for n in not_run:
        print(f"not run: {n}")
    print(f"price_check: {len(problems)} problem(s) across {len(checked)} check(s) run "
          f"({', '.join(checked) or 'none'}), {len(not_run)} not run")
    return 1 if problems else (2 if not_run else 0)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
