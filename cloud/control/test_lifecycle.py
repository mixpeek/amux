"""T-LIFE-1..6, T-DUN-1 and the cancellation path (docs/design/cloud-self-service.md, 3.2 and section 12).

Run: python3 -m unittest discover -s cloud/control -p 'test_*.py'
"""
import os, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lifecycle as lc  # noqa: E402
from fakes import FakeBilling, FakeInfra  # noqa: E402
from store import Store  # noqa: E402

DAY = lc.DAY
T0 = 1_800_000_000.0


class World:
    """A store with N customers, each with a server, a subscription and a fresh backup."""

    def __init__(self, n=10, shadow=False):
        self.dir = tempfile.mkdtemp()
        self.store = Store(os.path.join(self.dir, "c.db"))
        self.store.set_flag("lifecycle_shadow", "1" if shadow else "0")
        self.billing, self.infra = FakeBilling(), FakeInfra()
        self.now = T0
        for i in range(n):
            self.add(i)

    def add(self, i):
        org, sub = f"org{i}", f"sub{i}"
        h = self.infra.create_server(f"amux-c{i}", "cx33", "fsn1", "golden", {"amux-cloud": "1", "org": org}, "")
        self.store.run("INSERT INTO orgs(id,email,created_at) VALUES(?,?,?)", org, f"c{i}@example.com", T0)
        self.store.run("INSERT INTO subscriptions(id,org_id,plan,status,current_period_end,updated_at) VALUES(?,?,?,?,?,?)",
                       sub, org, "solo", "active", T0 + 20 * DAY, T0)
        self.store.run("INSERT INTO servers(id,org_id,subscription_id,slug,plan,hetzner_id,status,fleet_secret,created_at,"
                       "last_backup_at) VALUES(?,?,?,?,?,?,?,?,?,?)", f"srv{i}", org, sub, f"c{i}", "solo", h["id"],
                       "ready", "x", T0, T0)
        self.billing.subs[sub] = {"status": "active", "current_period_end": T0 + 20 * DAY}

    def fail_payment(self, i, at):
        """Both sources say the payment failed: our row (webhook) and the provider."""
        self.store.run("UPDATE subscriptions SET status='past_due', failure_started_at=? WHERE id=?", at, f"sub{i}")
        self.billing.subs[f"sub{i}"]["status"] = "past_due"

    def cancel(self, i, period_end):
        self.store.run("UPDATE subscriptions SET status='canceled', current_period_end=? WHERE id=?", period_end, f"sub{i}")
        self.billing.subs[f"sub{i}"].update(status="canceled", current_period_end=period_end)

    def at(self, t):
        self.now = t
        self.store.run("UPDATE servers SET last_backup_at=?", t - 60)   # the fleet agent keeps backing up
        return lc.sweep(self.store, self.billing, self.infra, now=t)

    def status(self, i):
        return self.store.one("SELECT status FROM servers WHERE id=?", f"srv{i}")["status"]


class LifeRules(unittest.TestCase):
    def test_T_LIFE_1_webhook_says_cancelled_but_api_says_active(self):
        w = World()
        w.store.run("UPDATE subscriptions SET status='canceled', current_period_end=? WHERE id='sub0'", T0)
        # provider still says active
        w.at(T0 + DAY)
        w.at(T0 + 4 * DAY + 1)
        self.assertEqual(w.status(0), "ready")
        self.assertEqual(w.infra.deleted, [])
        alerts = w.store.all("SELECT kind FROM alerts")
        self.assertIn("lifecycle_disagreement", [a["kind"] for a in alerts])

    def test_T_LIFE_1b_payment_after_failure_cancels_the_action(self):
        w = World()
        w.fail_payment(0, T0)
        w.at(T0 + DAY)
        w.store.run("INSERT INTO ledger(invoice_id,org_id,subscription_id,amount_cents,paid_at) VALUES('in1','org0','sub0',3900,?)",
                    T0 + 2 * DAY)
        w.at(T0 + 15 * DAY)
        self.assertEqual(w.status(0), "ready")
        self.assertEqual(w.infra.deleted, [])

    def test_T_LIFE_2_provider_unreachable_pauses_everything_destructive(self):
        for breakage in ("billing_unhealthy", "billing_reads_fail", "hetzner_unhealthy", "webhooks_silent"):
            with self.subTest(breakage=breakage):
                w = World()
                w.cancel(0, T0)
                if breakage == "billing_unhealthy":
                    w.billing.is_healthy = False
                elif breakage == "billing_reads_fail":
                    w.billing.fail_reads = True
                elif breakage == "hetzner_unhealthy":
                    w.infra.is_healthy = False
                else:
                    w.store.event("cloud.checkout_started")
                    w.store.run("UPDATE events SET ts=? WHERE name='cloud.checkout_started'", T0 + 4 * DAY - 3600)
                    w.store.run("INSERT INTO events(ts,name) VALUES(?,'stripe.webhook')", T0 - 2 * DAY)
                lc.sweep(w.store, w.billing, w.infra, now=T0 + DAY)
                s = w.at(T0 + 4 * DAY + 1)
                self.assertTrue(s["paused"], breakage)
                self.assertEqual(w.infra.deleted, [], breakage)
                self.assertEqual(w.status(0), "ready")

    def test_T_LIFE_2b_provisioning_is_not_paused_by_the_gate(self):
        # The gate only governs destructive steps; proposing (which only records) still happens.
        w = World()
        w.cancel(0, T0)
        w.billing.is_healthy = False
        s = w.at(T0 + DAY)
        self.assertEqual(s["proposed"], 1)

    def test_T_LIFE_3_small_fleet_single_deletion_proceeds(self):
        w = World(n=10)
        w.cancel(0, T0)
        w.at(T0 + DAY)
        s = w.at(T0 + 4 * DAY + 1)
        self.assertFalse(s["breaker"])
        self.assertEqual(s["executed"], 1)
        self.assertEqual(w.status(0), "deleted")

    def test_T_LIFE_3b_breaker_trips_and_deletes_none(self):
        w = World(n=10)
        for i in range(4):          # 4 > max(3, 5% of 10), all lapsing in one sweep
            w.cancel(i, T0)
        s = w.at(T0 + DAY)
        self.assertTrue(s["breaker"])
        self.assertEqual(w.store.all("SELECT * FROM lifecycle_actions"), [])
        s = w.at(T0 + 4 * DAY + 1)
        self.assertEqual(w.infra.deleted, [])
        self.assertEqual(w.store.flag("lifecycle_freeze"), "1")
        self.assertIn("lifecycle_breaker", [a["kind"] for a in w.store.all("SELECT kind FROM alerts")])
        s = w.at(T0 + 5 * DAY)      # stays frozen until a person clears it
        self.assertEqual(w.infra.deleted, [])

    def test_T_LIFE_3c_daily_cap_on_a_large_fleet(self):
        w = World(n=100)            # breaker limit is max(3, 5) = 5
        for i in range(5):
            w.cancel(i, T0)
        s = w.at(T0 + DAY)
        self.assertFalse(s["breaker"])
        w.at(T0 + 4 * DAY + 1)
        self.assertEqual(len(w.infra.deleted), 3)
        w.at(T0 + 5 * DAY + 2)
        self.assertEqual(len(w.infra.deleted), 5)
        holds = sorted(r["hold_until"] for r in w.store.all("SELECT hold_until FROM lifecycle_actions"))
        self.assertGreaterEqual(holds[3] - holds[0], DAY)

    def test_T_LIFE_4_no_deletion_without_verified_snapshot_or_fresh_backup(self):
        w = World()
        w.cancel(0, T0)
        w.at(T0 + DAY)
        w.infra.snapshot_status = "creating"
        w.at(T0 + 4 * DAY + 1)                                     # backup fresh, snapshot not ready
        self.assertEqual(w.status(0), "ready")
        w.store.run("UPDATE lifecycle_actions SET snapshot_id=NULL")
        w.infra.snapshot_status, w.infra.snapshot_size = "available", 0
        w.at(T0 + 4 * DAY + 2)                                     # snapshot listed but empty
        self.assertEqual(w.status(0), "ready")
        w.store.run("UPDATE lifecycle_actions SET snapshot_id=NULL")
        w.store.run("UPDATE servers SET last_backup_at=?", T0)      # backup now days old
        w.infra.snapshot_size = 2.5
        lc.sweep(w.store, w.billing, w.infra, now=T0 + 4 * DAY + 3)
        self.assertEqual(w.status(0), "ready")
        self.assertEqual(w.infra.deleted, [])
        w.at(T0 + 4 * DAY + 4)                                     # fresh backup, verified snapshot
        self.assertEqual(w.status(0), "deleted")
        a = w.store.one("SELECT snapshot_id FROM lifecycle_actions WHERE server_id='srv0' AND state='done'")
        self.assertIn(int(a["snapshot_id"]), w.infra.images)

    def test_T_LIFE_5_orphans_are_reported_and_left_running(self):
        w = World(n=2)
        unlabelled = w.infra.create_server("someone-elses", "cx22", "fsn1", "x", {}, "")
        orphan = w.infra.create_server("lost", "cx33", "fsn1", "x", {"amux-cloud": "1"}, "")
        s = w.at(T0 + DAY)
        self.assertEqual(s["orphans"], [orphan["id"]])
        self.assertIn(unlabelled["id"], w.infra.servers)
        self.assertIn(orphan["id"], w.infra.servers)

    def test_T_LIFE_6_shadow_records_and_replays(self):
        w = World(shadow=True)
        w.cancel(0, T0)
        w.cancel(1, T0)
        s = w.at(T0 + DAY)                  # shadow holds are the real ones, already past here
        self.assertEqual(s["shadowed"], 2)
        s = w.at(T0 + 4 * DAY + 1)
        self.assertEqual(s["shadowed"], 0)  # one shadow decision per server, never repeated
        self.assertEqual(w.infra.deleted, [])
        self.assertEqual(w.store.all("SELECT * FROM outbox"), [])           # nobody is warned for a shadow decision
        # customer 1 quietly pays again; the replay must flag the shadow decision as wrong
        w.store.run("INSERT INTO ledger(invoice_id,org_id,subscription_id,amount_cents,paid_at) VALUES('in9','org1','sub1',3900,?)",
                    T0 + 10 * DAY)
        w.billing.subs["sub1"].update(status="active", current_period_end=T0 + 40 * DAY)
        s = w.at(T0 + 19 * DAY)
        self.assertEqual(s["replayed"], 2)
        verdicts = sorted(r["replay_verdict"] for r in w.store.all("SELECT replay_verdict FROM lifecycle_actions WHERE replay_at IS NOT NULL"))
        self.assertEqual(verdicts, ["agree", "disagree"])
        self.assertIn("lifecycle_shadow_disagreement", [a["kind"] for a in w.store.all("SELECT kind FROM alerts")])
        w.at(T0 + 60 * DAY)
        self.assertEqual(w.store.flag("lifecycle_shadow"), "1")             # a disagreement keeps shadow on

    def test_T_LIFE_6b_shadow_ends_after_30_days_and_5_agreeing_replays(self):
        w = World(n=40, shadow=True)
        for i in range(5):
            w.cancel(i, T0)
        w.at(T0 + DAY)
        w.at(T0 + 2 * DAY)          # shadowed (holds already passed)
        self.assertEqual(w.store.flag("lifecycle_shadow"), "1")
        w.at(T0 + 17 * DAY)         # replays, but under 30 days
        self.assertEqual(w.store.flag("lifecycle_shadow"), "1")
        w.at(T0 + 33 * DAY)
        self.assertEqual(w.store.flag("lifecycle_shadow"), "0")
        # Now real: the lapsed customers get a fresh, warned, 3-day hold before anything is deleted.
        s = w.at(T0 + 33 * DAY + 60)
        self.assertEqual(w.infra.deleted, [])
        self.assertEqual(len(w.store.all("SELECT * FROM outbox WHERE template='server_will_be_snapshotted'")), 5)
        self.assertFalse(s["breaker"])          # re-proposals after shadow are expected, not an anomaly
        w.at(T0 + 36 * DAY + 120)
        self.assertEqual(len(w.infra.deleted), 3)       # holds spread: 3 due on the first day
        w.at(T0 + 37 * DAY + 240)
        self.assertEqual(len(w.infra.deleted), 5)

    def test_T_DUN_1_payment_failure_keeps_service_14_days(self):
        w = World()
        w.fail_payment(0, T0)
        w.at(T0 + DAY)
        w.at(T0 + 13 * DAY)
        self.assertEqual(w.status(0), "ready")
        w.at(T0 + 14 * DAY + 1)
        self.assertEqual(w.status(0), "deleted")
        warn = w.store.all("SELECT template FROM outbox WHERE to_addr='c0@example.com'")
        self.assertEqual([x["template"] for x in warn], ["server_will_be_snapshotted", "server_snapshotted"])

    def test_cancel_waits_for_period_end(self):
        w = World()
        w.cancel(0, T0 + 10 * DAY)          # cancelled now, paid through day 10
        w.at(T0 + DAY)
        self.assertEqual(w.store.all("SELECT * FROM lifecycle_actions"), [])
        w.at(T0 + 10 * DAY + 1)
        w.at(T0 + 13 * DAY + 2)
        self.assertEqual(w.status(0), "deleted")

    def _due_now(self, w, ids, at):
        """Simulate corrupted state: actions all due at once, bypassing the spread in propose()."""
        for i in ids:
            w.cancel(i, at - 10 * DAY)
            w.store.run("INSERT INTO lifecycle_actions(id,server_id,kind,reason,state,shadow,proposed_at,hold_until) "
                        "VALUES(?,?,?,?,?,?,?,?)", f"x{i}", f"srv{i}", "delete_server", "cancelled", "held", 0,
                        at - 5 * DAY, at - 1)

    def test_execution_breaker_is_a_backstop(self):
        w = World(n=10)
        self._due_now(w, range(4), T0 + 30 * DAY)                  # 4 due > max(3, 5% of 10)
        s = w.at(T0 + 30 * DAY)
        self.assertTrue(s["breaker"])
        self.assertEqual(w.infra.deleted, [])
        self.assertEqual(w.store.flag("lifecycle_freeze"), "1")

    def test_daily_cap_is_a_backstop(self):
        w = World(n=100)                                           # limit 5, so 4 due is no breaker
        self._due_now(w, range(4), T0 + 30 * DAY)
        s = w.at(T0 + 30 * DAY)
        self.assertFalse(s["breaker"])
        self.assertEqual(len(w.infra.deleted), 3)

    def test_freeze_switch_stops_deletions(self):
        w = World()
        w.cancel(0, T0)
        w.at(T0 + DAY)
        w.store.set_flag("lifecycle_freeze", "1")
        s = w.at(T0 + 4 * DAY + 1)
        self.assertEqual(w.infra.deleted, [])
        self.assertIn("lifecycle_freeze is on", s["paused"])


if __name__ == "__main__":
    unittest.main()
