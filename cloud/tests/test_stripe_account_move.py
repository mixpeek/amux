#!/usr/bin/env python3
"""Offline tests for cloud/control/stripe_account_move.py: the real CLI against a fake Stripe (two
accounts, told apart by key) and a fake Hetzner. Run: python3 cloud/tests/test_stripe_account_move.py
"""
import http.server, json, os, subprocess, sys, threading, time, unittest, urllib.parse

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLI = os.path.join(ROOT, "control", "stripe_account_move.py")
OLD, NEW = "sk_test_old", "sk_test_new"
END = int(time.time()) + 10 * 86400
HOOK = "https://cloud.amux.io/webhooks/stripe"


def price(key, amount):
    return {"id": f"price_{key}", "lookup_key": key, "unit_amount": amount}


def sub(id, cust, key, amount, slug=None, **kw):
    return dict({"id": id, "customer": cust, "status": "active", "metadata": {"slug": slug} if slug else {},
                 "items": {"data": [{"price": price(key, amount), "quantity": 1, "current_period_end": END}]},
                 "collection_method": "charge_automatically", "cancel_at_period_end": False}, **kw)


def world():
    return {
        OLD: {"account": "acct_old", "prices": {}, "customers": {},
              "subs": {s["id"]: s for s in [
                  sub("sub_a", "cus_a", "amux_cloud_team_monthly", 6900, "alpha"),
                  sub("sub_b", "cus_b", "amux_cloud_solo_monthly", 3900, "beta", cancel_at_period_end=True),
                  sub("sub_m", "cus_m", "mixpeek_pro", 50000),
                  sub("sub_c", "cus_c", "amux_cloud_solo_monthly", 3900, "gone", status="canceled")]},
              "hooks": [{"id": "we_amux", "url": HOOK}, {"id": "we_mixpeek", "url": "https://api.mixpeek.com/stripe"}]},
        NEW: {"account": "acct_new", "subs": {}, "hooks": [],
              "prices": {k: price(k, a) | {"id": f"price_new_{k}"} for k, a in
                         [("amux_cloud_solo_monthly", 3900), ("amux_cloud_team_monthly", 6900), ("amux_cloud_max_monthly", 12900)]},
              "customers": {c: {"id": c, "invoice_settings": {"default_payment_method": f"pm_{c}"}} for c in ("cus_a", "cus_b", "cus_c")}},
        "servers": [{"id": 1, "labels": {"amux-standalone": "1", "amux-mode": "test", "amux-sub": "sub_a", "amux-slug": "alpha", "amux-ready": "1"}},
                    {"id": 2, "labels": {"amux-standalone": "1", "amux-mode": "test", "amux-sub": "sub_b", "amux-slug": "beta", "amux-ready": "1"}}],
        "images": [{"id": 9, "labels": {"amux-cloud-final": "1", "amux-sub": "sub_c", "amux-slug": "gone"}}],
        "writes": [], "n": 0,
    }


def unflatten(pairs):
    out = {}
    for k, v in pairs:
        parts = k.replace("]", "").split("[")
        d = out
        for p in parts[:-1]:
            d = d.setdefault(p, {})
        d[parts[-1]] = v
    return out


class Fake(http.server.BaseHTTPRequestHandler):
    W = None

    def log_message(self, *a):
        pass

    def reply(self, code, body):
        b = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def do_PUT(self):
        self.route("PUT")

    def do_DELETE(self):
        self.route("DELETE")

    def route(self, m):
        W = self.W
        u = urllib.parse.urlparse(self.path)
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0)).decode()
        q = unflatten(urllib.parse.parse_qsl(u.query))
        key = self.headers["Authorization"].split()[1]
        if u.path.startswith("/hetzner"):
            p = u.path[len("/hetzner"):]
            if m == "PUT":
                body = json.loads(raw)
                kind, id = p.strip("/").split("/")
                obj = next(x for x in W[kind] if x["id"] == int(id))
                obj["labels"] = body["labels"]
                W["writes"].append(("hetzner", m, p))
                return self.reply(200, {kind[:-1]: obj})
            sel = [x.split("=") for x in q.get("label_selector", "").split(",") if x]
            kind = p.strip("/")
            return self.reply(200, {kind: [x for x in W[kind] if all(x["labels"].get(k) == v for k, v in sel)],
                                    "meta": {"pagination": {"next_page": None}}})
        a = W[key]
        p = u.path[len("/stripe"):]
        f = unflatten(urllib.parse.parse_qsl(raw, keep_blank_values=True))
        if m != "GET":
            W["writes"].append((a["account"], m, p))
        parts = p.strip("/").split("/")
        if p == "/account":
            return self.reply(200, {"id": a["account"]})
        if p == "/prices":
            k = q["lookup_keys"]["0"]
            return self.reply(200, {"data": [a["prices"][k]] if k in a["prices"] else []})
        if p == "/webhook_endpoints":
            return self.reply(200, {"data": a["hooks"]})
        if parts[0] == "webhook_endpoints" and m == "DELETE":
            a["hooks"] = [h for h in a["hooks"] if h["id"] != parts[1]]
            return self.reply(200, {"id": parts[1], "deleted": True})
        if parts[0] == "customers":
            c = a["customers"].get(parts[1])
            return self.reply(200, c) if c else self.reply(404, {"error": {"message": "No such customer"}})
        if p == "/payment_methods":
            return self.reply(200, {"data": []})
        if p == "/subscriptions" and m == "GET":
            if q.get("customer") and q["customer"] not in a["customers"]:
                return self.reply(400, {"error": {"message": f"No such customer: '{q['customer']}'"}})   # as real Stripe
            rows = [s for s in a["subs"].values() if (not q.get("customer") or s["customer"] == q["customer"])]
            return self.reply(200, {"data": rows, "has_more": False})
        if p == "/subscriptions" and m == "POST":
            W["n"] += 1
            idem = self.headers.get("Idempotency-Key")
            s = {"id": f"sub_new{W['n']}", "customer": f["customer"], "status": "trialing" if f.get("trial_end") else "active",
                 "metadata": f.get("metadata", {}), "params": f, "idem": idem,
                 "cancel_at_period_end": f.get("cancel_at_period_end") == "true",
                 "items": {"data": [{"price": next(pr for pr in a["prices"].values() if pr["id"] == f["items"]["0"]["price"]),
                                     "quantity": int(f["items"]["0"]["quantity"])}]}}
            a["subs"][s["id"]] = s
            return self.reply(200, s)
        if parts[0] == "subscriptions":
            s = a["subs"][parts[1]]
            if m == "POST":
                if "cancel_at_period_end" in f:
                    s["cancel_at_period_end"] = f["cancel_at_period_end"] == "true"
                for k, v in f.get("metadata", {}).items():
                    if v == "":
                        s["metadata"].pop(k, None)
                    else:
                        s["metadata"][k] = v
            if m == "DELETE":
                s["status"] = "canceled"
            return self.reply(200, s)
        return self.reply(404, {"error": {"message": f"fake: {m} {p}"}})


class Move(unittest.TestCase):
    def setUp(self):
        self.w = world()
        Fake.W = self.w
        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fake)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{self.srv.server_port}"
        self.env = dict(os.environ, AMUX_STRIPE_API_BASE=base + "/stripe", AMUX_HETZNER_API_BASE=base + "/hetzner",
                        STRIPE_TEST_SECRET_KEY=OLD, AMUX_CLOUD_STRIPE_TEST_SECRET_KEY=NEW, HCLOUD_TOKEN="hz", HOME="/nonexistent")

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()

    def run_cli(self, *args, ok=True):
        r = subprocess.run([sys.executable, CLI, *args, "--test"], env=self.env, capture_output=True, text=True, timeout=60)
        if ok:
            self.assertEqual(r.returncode, 0, r.stderr)
        return r

    def writes_to(self, account):
        return [w for w in self.w["writes"] if w[0] == account]

    def test_plan_reads_only_and_leaves_mixpeek_unlisted(self):
        p = json.loads(self.run_cli("plan").stdout)
        self.assertEqual([p["amux_subscriptions"], p["left_alone_not_amux"], p["blocked"], p["amux_mrr_usd"]], [2, 1, 0, 108.0])
        self.assertEqual(p["old_webhooks_to_retire"], ["we_amux"])
        self.assertNotIn("sub_m", json.dumps(p))
        self.assertEqual(self.w["writes"], [])

    def test_writes_need_the_owners_go(self):
        for cmd in ("apply", "relabel", "retire-old-webhook", "rollback"):
            r = self.run_cli(cmd, ok=False)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("--approved", r.stderr)
        self.assertEqual(self.w["writes"], [])

    def test_apply_bills_from_the_old_period_end_and_ends_the_old_one(self):
        out = json.loads(self.run_cli("apply", "--approved", "MSG-1").stdout)["result"]
        new = {s["metadata"]["moved_from"]: s for s in self.w[NEW]["subs"].values()}
        a, b = new["sub_a"], new["sub_b"]
        self.assertEqual([a["params"].get("billing_cycle_anchor"), a["params"].get("proration_behavior"), a["status"]], [str(END), "none", "active"])
        self.assertEqual([b["params"].get("trial_end"), b["cancel_at_period_end"]], [str(END), True])   # was ending: never charged
        self.assertEqual([a["params"]["default_payment_method"], a["items"]["data"][0]["price"]["id"]], ["pm_cus_a", "price_new_amux_cloud_team_monthly"])
        self.assertEqual(a["idem"], "amux-move-sub_a")
        self.assertEqual(a["metadata"]["slug"], "alpha")
        old = self.w[OLD]["subs"]
        self.assertEqual([old["sub_a"]["cancel_at_period_end"], old["sub_a"]["metadata"]["moved_to"], old["sub_a"]["status"]], [True, a["id"], "active"])
        self.assertFalse(any("sub_m" in w[2] for w in self.w["writes"]), "the Mixpeek subscription is never written")
        self.assertEqual(sorted(o["action"] for o in out), ["moved", "moved"])
        again = json.loads(self.run_cli("apply", "--approved", "MSG-1").stdout)["result"]
        self.assertEqual([o["action"] for o in again], ["already moved", "already moved"])
        self.assertEqual(len(self.w[NEW]["subs"]), 2)

    def test_a_blocked_subscription_stops_apply_before_any_write(self):
        del self.w[NEW]["customers"]["cus_a"]                      # the card copy has not run for alpha
        p = json.loads(self.run_cli("plan").stdout)
        self.assertEqual(p["blocked"], 1)
        self.assertIn("not in the new account", json.dumps(p["rows"]))
        r = self.run_cli("apply", "--approved", "MSG-1", ok=False)
        self.assertIn("blocked", r.stderr)
        self.assertEqual(self.w["writes"], [])

    def test_unfinished_signup_and_missing_price_block(self):
        self.w[OLD]["subs"]["sub_a"]["metadata"]["pw_hash"] = "scrypt$x"
        del self.w[NEW]["prices"]["amux_cloud_solo_monthly"]
        rows = {r["old"]: r["blocked"] for r in json.loads(self.run_cli("plan").stdout)["rows"]}
        self.assertTrue(any("signup unfinished" in w for w in rows["sub_a"]))
        self.assertTrue(any("run stripe_setup.py there" in w for w in rows["sub_b"]))

    def test_relabel_retire_and_rollback(self):
        self.run_cli("apply", "--approved", "MSG-1")
        res = json.loads(self.run_cli("relabel", "--approved", "MSG-1").stdout)["result"]
        subs = {s["metadata"]["moved_from"]: s["id"] for s in self.w[NEW]["subs"].values()}
        self.assertEqual({s["labels"]["amux-slug"]: s["labels"]["amux-sub"] for s in self.w["servers"]},
                         {"alpha": subs["sub_a"], "beta": subs["sub_b"]})
        self.assertEqual(res["snapshots"], [{"image": 9, "customer": "cus_c"}])
        self.assertEqual(json.loads(self.run_cli("plan").stdout)["blocked"], 0)     # moved rows stay clean
        self.run_cli("retire-old-webhook", "--approved", "MSG-1")
        self.assertEqual([h["id"] for h in self.w[OLD]["hooks"]], ["we_mixpeek"])
        self.run_cli("rollback", "--approved", "MSG-1")
        old = self.w[OLD]["subs"]
        self.assertEqual([old["sub_a"]["cancel_at_period_end"], old["sub_b"]["cancel_at_period_end"]], [False, True])
        self.assertNotIn("moved_to", old["sub_a"]["metadata"])
        self.assertEqual({s["status"] for s in self.w[NEW]["subs"].values()}, {"canceled"})
        self.assertEqual({s["labels"]["amux-sub"] for s in self.w["servers"]}, {"sub_a", "sub_b"})

    def test_rollback_after_an_old_period_ended_changes_nothing(self):
        self.run_cli("apply", "--approved", "MSG-1")
        self.run_cli("relabel", "--approved", "MSG-1")
        self.w[OLD]["subs"]["sub_a"]["status"] = "canceled"          # alpha's old period is over
        before = len(self.w["writes"])
        r = self.run_cli("rollback", "--approved", "MSG-1", ok=False)
        self.assertIn("too late to roll back: sub_a", r.stderr)
        self.assertEqual(self.w["writes"][before:], [])
        self.assertTrue(all(s["labels"]["amux-sub"].startswith("sub_new") for s in self.w["servers"]))

    def test_one_account_under_both_names_is_refused(self):
        self.env["AMUX_CLOUD_STRIPE_TEST_SECRET_KEY"] = OLD
        r = self.run_cli("plan", ok=False)
        self.assertIn("same Stripe account", r.stderr)


if __name__ == "__main__":
    unittest.main()
