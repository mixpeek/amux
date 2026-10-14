"""Unit tests for price_check.py. Run: python3 -m unittest cloud/control/test_price_check.py"""
import os, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import price_check as pc  # noqa: E402

PLANS = pc.load_plans()
LOCS = PLANS["locations"]


def hetzner(prices):
    return {st: {loc: usd for loc in LOCS} for st, usd in prices.items()}


class Margin(unittest.TestCase):
    def test_current_prices_pass_at_measured_cost(self):
        self.assertEqual(pc.check_margin(PLANS, hetzner({"cx33": 9.99, "cx43": 18.49, "cx53": 34.99})), [])

    def test_a_price_rise_trips_the_guard(self):
        problems = pc.check_margin(PLANS, hetzner({"cx33": 19.99, "cx43": 18.49, "cx53": 34.99}))
        self.assertEqual(len(problems), len(LOCS))
        self.assertIn("solo", problems[0])

    def test_missing_location_is_reported(self):
        prices = hetzner({"cx33": 9.99, "cx43": 18.49, "cx53": 34.99})
        del prices["cx43"]["hel1"]
        self.assertEqual(pc.check_margin(PLANS, prices), ["team: cx43 not offered in hel1"])


class Site(unittest.TestCase):
    def html(self, **over):
        prices = {p["id"]: p["price_usd"] for p in PLANS["plans"]}
        prices.update(over)
        return "".join(f'<span data-plan-price="{k}:{v}">${v}</span>' for k, v in prices.items())

    def test_matching_page_passes(self):
        self.assertEqual(pc.check_site(PLANS, self.html()), [])

    def test_wrong_price_fails(self):
        self.assertEqual(pc.check_site(PLANS, self.html(solo=26)),
                         ["site: solo shows $26, plans.json says $39"])

    def test_missing_and_unknown_plans_fail(self):
        html = self.html().replace('data-plan-price="max:129"', 'data-plan-price="ultra:999"')
        problems = sorted(pc.check_site(PLANS, html))
        self.assertEqual(problems, ["site: data-plan-price for unknown plan ultra", "site: no data-plan-price for max"])


class Stripe(unittest.TestCase):
    def prices(self):
        return {p["stripe_lookup_key"]: {"unit_amount": p["price_usd"] * 100, "currency": "usd", "recurring": "month"}
                for p in PLANS["plans"]}

    def test_matching_prices_pass(self):
        self.assertEqual(pc.check_stripe(PLANS, self.prices()), [])

    def test_amount_interval_and_missing_fail(self):
        sp = self.prices()
        sp["amux_cloud_solo_monthly"]["unit_amount"] = 2600
        sp["amux_cloud_team_monthly"]["recurring"] = "year"
        del sp["amux_cloud_max_monthly"]
        self.assertEqual(len(pc.check_stripe(PLANS, sp)), 3)


if __name__ == "__main__":
    unittest.main()
