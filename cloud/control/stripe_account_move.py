#!/usr/bin/env python3
"""Move amux Cloud billing from the Stripe account it shares with Mixpeek to its own account.

Owner ask (2026-11-21, via mixpeek-finances): amux Cloud gets its own Stripe account under
Mixpeek Inc., so its books are easy to tell from Mixpeek's. Runbook: cloud/start/STRIPE-ACCOUNT-MOVE.md.

  stripe_account_move.py plan                       read-only: what would move, what blocks it
  stripe_account_move.py apply    --approved <ref>  recreate each subscription in the new account,
                                                    billing from the end of its paid period; the old
                                                    one ends at that period end
  stripe_account_move.py relabel  --approved <ref>  point each server at its new subscription
  stripe_account_move.py retire-old-webhook --approved <ref>
  stripe_account_move.py rollback --approved <ref>  undo apply and relabel (before any old period ends)

Keys come from the environment or ~/.amux/server.env: STRIPE_LIVE_SECRET_KEY (the shared account,
the old one), AMUX_CLOUD_STRIPE_LIVE_SECRET_KEY (the new one), HCLOUD_TOKEN. --test uses
STRIPE_TEST_SECRET_KEY and AMUX_CLOUD_STRIPE_TEST_SECRET_KEY instead, to rehearse on two test-mode
accounts. Every write needs --approved naming the owner's go (a MSG id); without it nothing changes.

A subscription is amux Cloud's when it carries metadata.slug and every price on it has an amux
Cloud lookup key (plans.json). Anything else in the shared account is Mixpeek's: it is counted,
never listed, never touched.
"""
import argparse, json, os, sys, time, urllib.error, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
STRIPE = os.environ.get("AMUX_STRIPE_API_BASE", "https://api.stripe.com/v1")
HETZNER = os.environ.get("AMUX_HETZNER_API_BASE", "https://api.hetzner.cloud/v1")
LIVE = ("active", "trialing", "past_due")
WEBHOOK_URL = "https://cloud.amux.io/webhooks/stripe"


def envv(name):
    if os.environ.get(name):
        return os.environ[name]
    try:
        for line in open(os.path.expanduser("~/.amux/server.env")):
            k, _, v = line.strip().removeprefix("export ").partition("=")
            if k == name:
                return v.strip().strip('"').strip("'")
    except FileNotFoundError:
        pass
    return ""


def form(d, prefix=""):
    out = []
    for k, v in d.items():
        key = f"{prefix}[{k}]" if prefix else k
        if isinstance(v, dict):
            out += form(v, key)
        elif isinstance(v, list):
            for i, x in enumerate(v):
                out += form(x, f"{key}[{i}]") if isinstance(x, dict) else [(f"{key}[{i}]", str(x))]
        elif v is not None:
            out.append((key, str(v)))
    return out


def http(method, url, token, params=None, body=None, idem=None):
    headers = {"Authorization": f"Bearer {token}"}
    data = None
    if body is not None:
        data, headers["Content-Type"] = json.dumps(body).encode(), "application/json"
    elif method in ("POST", "DELETE") and params:
        data = urllib.parse.urlencode(form(params)).encode()
    elif params:
        url += "?" + urllib.parse.urlencode(form(params))
    if idem:
        headers["Idempotency-Key"] = idem
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        raise SystemExit(f"{method} {url.split('?')[0]}: HTTP {e.code} {e.read()[:300]!r}")


class Stripe:
    def __init__(self, key, name):
        self.key, self.name = key, name

    def call(self, method, path, params=None, idem=None):
        return http(method, STRIPE + path, self.key, params, idem=idem)

    def all(self, path, params):
        out, after = [], None
        for _ in range(200):
            page = self.call("GET", path, dict(params, limit=100, **({"starting_after": after} if after else {})))
            out += page["data"]
            if not page.get("has_more") or not page["data"]:
                return out
            after = page["data"][-1]["id"]
        raise SystemExit(f"{self.name}: more than 20,000 rows at {path}; refusing to guess")


def hetzner(token, method, path, params=None, body=None):
    return http(method, HETZNER + path, token, params, body=body)


def hz_all(token, path, key, params):
    out, page = [], 1
    while page:
        d = hetzner(token, "GET", path, dict(params, per_page=50, page=page))
        out += d[key]
        page = ((d.get("meta") or {}).get("pagination") or {}).get("next_page")
    return out


def servers_of(hz, mode):
    return hz_all(hz, "/servers", "servers", {"label_selector": f"amux-standalone=1,amux-mode={mode}"})


def final_snapshots(hz):
    return hz_all(hz, "/images", "images", {"type": "snapshot", "label_selector": "amux-cloud-final=1"})


def period_end(sub):
    # API versions from 2025-03 keep the period on each item, older ones on the subscription.
    items = (sub.get("items") or {}).get("data") or []
    return sub.get("current_period_end") or (items[0].get("current_period_end") if items else None)


def lookup_keys():
    return {p["stripe_lookup_key"] for p in json.load(open(os.path.join(HERE, "plans.json")))["plans"]}


def is_amux(sub, keys):
    items = sub["items"]["data"]
    return bool(items) and all((i["price"].get("lookup_key") or "") in keys for i in items)


def moved_counterpart(new, cust, old_id):
    for s in new.all("/subscriptions", {"customer": cust, "status": "all"}):
        if (s.get("metadata") or {}).get("moved_from") == old_id and s["status"] != "canceled":
            return s
    return None


def plan(old, new, hz, mode):
    """Everything apply would do, and every reason it would refuse. Reads only."""
    keys = lookup_keys()
    if old.call("GET", "/account")["id"] == new.call("GET", "/account")["id"]:
        raise SystemExit("the old and new keys belong to the same Stripe account")
    new_prices = {}
    for k in keys:
        got = new.call("GET", "/prices", {"lookup_keys": [k], "active": "true"})["data"]
        if got:
            new_prices[k] = got[0]
    subs = old.all("/subscriptions", {"status": "all", "expand": ["data.discount"]})
    servers = servers_of(hz, mode)
    rows, others, blocked = [], 0, []
    for s in subs:
        if s["status"] not in LIVE:
            continue
        meta = s.get("metadata") or {}
        amux = is_amux(s, keys)
        if not amux and not meta.get("slug"):
            others += 1                       # Mixpeek's: counted, never listed
            continue
        cust = s["customer"] if isinstance(s["customer"], str) else s["customer"]["id"]
        try:
            c = new.call("GET", f"/customers/{cust}")
        except SystemExit:                    # Stripe answers 404 until the card copy lands
            c = None
        done = moved_counterpart(new, cust, s["id"]) if c and not c.get("deleted") else None
        why = []
        if done:
            rows.append({"old": s["id"], "slug": meta.get("slug"), "customer": cust, "new": done["id"], "blocked": [],
                         "items": [{"lookup_key": i["price"].get("lookup_key"), "quantity": i.get("quantity", 1),
                                    "unit_amount": i["price"].get("unit_amount")} for i in s["items"]["data"]]})
            continue
        if not amux:
            why.append("has metadata.slug but a price that is not an amux Cloud price")
        if not meta.get("slug"):
            why.append("amux Cloud price but no metadata.slug (no server it belongs to)")
        if meta.get("pw_hash"):
            why.append("signup unfinished (pw_hash still set): wait for its server")
        if s["status"] == "past_due":
            why.append("past_due: settle or cancel it in the old account first")
        if s.get("discount") and (s["discount"].get("coupon") or {}).get("duration") != "once":
            why.append(f"carries a lasting discount ({s['discount']['coupon'].get('id')}): decide it by hand")
        for i in s["items"]["data"]:
            if (i["price"].get("lookup_key") or "") in keys and i["price"]["lookup_key"] not in new_prices:
                why.append(f"new account has no price {i['price']['lookup_key']}: run stripe_setup.py there")
        mine = [x for x in servers if x["labels"].get("amux-sub") == s["id"]]
        if not mine:
            why.append("no server carries this subscription")
        if any(x["labels"].get("amux-ready") != "1" for x in mine):
            why.append("its server is not ready yet")
        pm = None
        if not c or c.get("deleted"):
            why.append(f"customer {cust} is not in the new account (the card copy has not run)")
        else:
            pm = (c.get("invoice_settings") or {}).get("default_payment_method") or c.get("default_source")
            if not pm:
                cards = new.call("GET", "/payment_methods", {"customer": cust, "type": "card"})["data"]
                pm = cards[0]["id"] if len(cards) == 1 else None
                if not pm:
                    why.append(f"customer {cust} has {len(cards)} cards in the new account and no default")
        end = s.get("trial_end") if s["status"] == "trialing" else period_end(s)
        if not end or end < time.time() + 3600:
            why.append("its period ends within the hour: move it after it renews")
        row = {"old": s["id"], "slug": meta.get("slug"), "customer": cust, "status": s["status"],
               "starts_billing": end, "cancel_at_period_end": bool(s.get("cancel_at_period_end")),
               "items": [{"lookup_key": i["price"].get("lookup_key"), "quantity": i.get("quantity", 1),
                          "unit_amount": i["price"].get("unit_amount")} for i in s["items"]["data"]],
               "payment_method": pm, "servers": [x["id"] for x in mine], "new": None,
               "collection_method": s.get("collection_method"), "automatic_tax": (s.get("automatic_tax") or {}).get("enabled", False),
               "blocked": why}
        rows.append(row)
        if why:
            blocked.append(row)
    mrr = sum(i["unit_amount"] * i["quantity"] for r in rows for i in r["items"] if i["unit_amount"]) / 100
    hooks = [h["id"] for h in old.call("GET", "/webhook_endpoints", {"limit": 100})["data"] if h["url"] == WEBHOOK_URL]
    snaps = final_snapshots(hz)
    return {"measured": True, "old_account_subscriptions_considered": len(subs), "left_alone_not_amux": others,
            "amux_subscriptions": len(rows), "blocked": len(blocked), "amux_mrr_usd": mrr,
            "new_prices": sorted(new_prices), "old_webhooks_to_retire": hooks,
            "final_snapshots_to_tag": len([i for i in snaps if not i["labels"].get("amux-cust")]), "rows": rows}


def need_approval(a):
    if not (a.approved or "").strip():
        raise SystemExit(f"{a.cmd} changes Stripe or Hetzner: pass --approved <the owner's go, e.g. MSG-123>")


def apply(old, new, hz, mode, approved):
    p = plan(old, new, hz, mode)
    if p["blocked"]:
        raise SystemExit(f"{p['blocked']} subscription(s) blocked; run plan and clear them first. Nothing changed.")
    out = []
    for r in p["rows"]:
        if r["new"]:
            out.append({"old": r["old"], "new": r["new"], "action": "already moved"})
            continue
        prices = {k: new.call("GET", "/prices", {"lookup_keys": [k], "active": "true"})["data"][0]["id"]
                  for k in {i["lookup_key"] for i in r["items"]}}
        oldsub = old.call("GET", f"/subscriptions/{r['old']}")
        meta = {k: v for k, v in (oldsub.get("metadata") or {}).items() if k not in ("pw_hash", "alerted")}
        meta.update(moved_from=r["old"], moved_old_cancel=("1" if r["cancel_at_period_end"] else "0"), moved_approved=approved[:100])
        params = {"customer": r["customer"], "default_payment_method": r["payment_method"], "metadata": meta,
                  "items": [{"price": prices[i["lookup_key"]], "quantity": i["quantity"]} for i in r["items"]],
                  "collection_method": r["collection_method"] or "charge_automatically",
                  "automatic_tax": {"enabled": "true" if r["automatic_tax"] else "false"}}
        if r["cancel_at_period_end"]:
            # Already ending: no charge in the new account either; it ends when the old one would.
            params.update(trial_end=r["starts_billing"], cancel_at_period_end="true")
        else:
            # Free until the old paid period ends, then billed on the same day each month.
            params.update(billing_cycle_anchor=r["starts_billing"], proration_behavior="none")
        n = new.call("POST", "/subscriptions", params, idem=f"amux-move-{r['old']}")
        old.call("POST", f"/subscriptions/{r['old']}", {"cancel_at_period_end": "true", "metadata": {"moved_to": n["id"]}})
        out.append({"old": r["old"], "new": n["id"], "slug": r["slug"], "status": n["status"], "action": "moved"})
        print(json.dumps({"event": "stripe_move.moved", **out[-1]}), file=sys.stderr)
    return out


def pairs(new):
    return [s for s in new.all("/subscriptions", {"status": "all"})
            if (s.get("metadata") or {}).get("moved_from") and s["status"] != "canceled"]


def relabel(new, hz, mode, back=False):
    done = []
    servers = servers_of(hz, mode)
    for s in pairs(new):
        old_id, new_id = s["metadata"]["moved_from"], s["id"]
        src, dst = (new_id, old_id) if back else (old_id, new_id)
        for srv in servers:
            if srv["labels"].get("amux-sub") != src:
                continue
            labels = dict(srv["labels"], **{"amux-sub": dst})
            if back:
                labels.pop("amux-moved-from", None)
            else:
                labels["amux-moved-from"] = old_id
            hetzner(hz, "PUT", f"/servers/{srv['id']}", body={"labels": labels})
            done.append({"server": srv["id"], "slug": srv["labels"].get("amux-slug"), "from": src, "to": dst})
    return done


def tag_snapshots(old, hz):
    """A final snapshot names its (old, ended) subscription. Give it the customer too, so a
    customer who comes back within 30 days still gets their server restored from the new account."""
    done = []
    for i in final_snapshots(hz):
        if i["labels"].get("amux-cust") or not i["labels"].get("amux-sub"):
            continue
        s = old.call("GET", f"/subscriptions/{i['labels']['amux-sub']}")
        cust = s["customer"] if isinstance(s["customer"], str) else s["customer"]["id"]
        hetzner(hz, "PUT", f"/images/{i['id']}", body={"labels": dict(i["labels"], **{"amux-cust": cust})})
        done.append({"image": i["id"], "customer": cust})
    return done


def rollback(old, new, hz, mode):
    # Every old subscription must still be live before anything changes: a rollback that stopped
    # halfway would leave a server pointing at a subscription that is about to end.
    moved = pairs(new)
    ended = [f"{s['metadata']['moved_from']} ({o['status']})" for s in moved
             for o in [old.call("GET", f"/subscriptions/{s['metadata']['moved_from']}")] if o["status"] not in LIVE]
    if ended:
        raise SystemExit(f"too late to roll back: {', '.join(ended)} already ended. Nothing changed.")
    back = relabel(new, hz, mode, back=True)
    out = []
    for s in moved:
        old_id = s["metadata"]["moved_from"]
        old.call("POST", f"/subscriptions/{old_id}", {"cancel_at_period_end": "true" if s["metadata"].get("moved_old_cancel") == "1" else "false",
                                                       "metadata": {"moved_to": ""}})
        new.call("DELETE", f"/subscriptions/{s['id']}", {"invoice_now": "false", "prorate": "false"})
        out.append({"old": old_id, "new": s["id"], "action": "rolled back"})
    return {"servers": back, "subscriptions": out}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("cmd", choices=["plan", "apply", "relabel", "retire-old-webhook", "rollback"])
    ap.add_argument("--approved", default="", help="the owner's go for a write, e.g. MSG-123")
    ap.add_argument("--test", action="store_true", help="rehearse on two test-mode accounts")
    a = ap.parse_args()
    mode, pre = ("test", "sk_test_") if a.test else ("live", "sk_live_")
    oldk = envv(f"STRIPE_{mode.upper()}_SECRET_KEY")
    newk = envv(f"AMUX_CLOUD_STRIPE_{mode.upper()}_SECRET_KEY")
    hz = envv("HCLOUD_TOKEN")
    for name, k in (("old", oldk), ("new", newk)):
        if not k.startswith((pre, pre.replace("sk_", "rk_"))):
            raise SystemExit(f"the {name} account's {mode} key is missing or not a {mode} key")
    if not hz:
        raise SystemExit("HCLOUD_TOKEN is missing")
    old, new = Stripe(oldk, "old"), Stripe(newk, "new")
    if a.cmd == "plan":
        print(json.dumps(plan(old, new, hz, mode), indent=1))
        return 0
    need_approval(a)
    if a.cmd == "apply":
        res = apply(old, new, hz, mode, a.approved)
    elif a.cmd == "relabel":
        res = {"servers": relabel(new, hz, mode), "snapshots": tag_snapshots(old, hz)}
    elif a.cmd == "retire-old-webhook":
        res = [old.call("DELETE", f"/webhook_endpoints/{h['id']}") and h["id"]
               for h in old.call("GET", "/webhook_endpoints", {"limit": 100})["data"] if h["url"] == WEBHOOK_URL]
    else:
        res = rollback(old, new, hz, mode)
    print(json.dumps({"cmd": a.cmd, "approved": a.approved, "result": res}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
