"""Abuse handling (docs/design/cloud-self-service.md M17).

Signals arrive from two places: the fleet agent's heartbeat (CPU over the last
six hours and the names of known miners it sees running) and abuse reports
(Hetzner's abuse mail, filed through the admin API by whatever reads abuse@).

  clear   a provider abuse report, or a known miner running: isolate at once.
          Isolation is a Hetzner firewall that denies all outbound traffic except
          HTTPS to the control plane (so heartbeats, backups and the customer's
          appeal keep working) plus a "stop workers" command to the fleet agent.
          The customer is emailed why and how to appeal.
  likely  sustained full CPU with no known cause: notify the customer, record it,
          and change nothing on the server.
  noise   anything else.

Deleting a server or its data for abuse is never automatic; it is the owner's
call (L3), and section 9's default after 7 days is to keep the server isolated
and its data intact.
"""
import json, time

MINERS = {"xmrig", "xmr-stak", "minerd", "cpuminer", "ethminer", "nbminer", "t-rex", "lolminer", "phoenixminer",
          "nanominer", "gminer", "teamredminer", "srbminer", "kdevtmpfsi", "kinsing"}
SUSTAINED_CPU = 95.0


def classify(signals):
    """signals: {cpu_pct_6h, processes: [names], report: {source, detail}}."""
    procs = {p.lower() for p in signals.get("processes") or []}
    miners = sorted(procs & MINERS)
    if signals.get("report"):
        return "clear", f"abuse report from {signals['report'].get('source', 'provider')}"
    if miners:
        return "clear", "known miner running: " + ", ".join(miners)
    if (signals.get("cpu_pct_6h") or 0) >= SUSTAINED_CPU:
        return "likely", f"CPU at {signals['cpu_pct_6h']:.0f}% for six hours"
    return "noise", ""


def handle(store, infra, srv, signals, control_ips, now=None):
    """Returns the verdict. Isolation is idempotent and recorded as events."""
    now = now or time.time()
    verdict, why = classify(signals)
    if verdict == "noise":
        return verdict
    store.event("abuse.signal", srv["org_id"], server=srv["id"], verdict=verdict, why=why)
    org = store.one("SELECT email FROM orgs WHERE id=?", srv["org_id"])
    if verdict == "likely":
        store.enqueue_email(org["email"], "abuse_notice", dedupe=f"abuse-likely:{srv['id']}:{int(now // 86400)}",
                            slug=srv["slug"], why=why, isolated=False)
        return verdict
    if store.one("SELECT 1 x FROM events WHERE name='abuse.isolated' AND props LIKE ?", f'%"server": "{srv["id"]}"%') and \
            not store.one("SELECT 1 x FROM events WHERE name='abuse.lifted' AND props LIKE ? AND ts > (SELECT MAX(ts) FROM "
                          "events WHERE name='abuse.isolated' AND props LIKE ?)", f'%"server": "{srv["id"]}"%', f'%"server": "{srv["id"]}"%'):
        return "already_isolated"
    infra.isolate(srv["hetzner_id"], control_ips)
    store.event("abuse.isolated", srv["org_id"], server=srv["id"], why=why)
    store.enqueue_email(org["email"], "abuse_notice", dedupe=f"abuse-isolated:{srv['id']}:{int(now)}", slug=srv["slug"],
                        why=why, isolated=True)
    store.alert("abuse_isolated", f"{srv['slug']} isolated: {why}. Its data is kept; deleting it is your decision.")
    return "isolated"


def lift(store, infra, srv, reason):
    """After a successful appeal: remove the firewall; the workers restart when the customer starts them."""
    infra.unisolate(srv["hetzner_id"])
    store.event("abuse.lifted", srv["org_id"], server=srv["id"], reason=reason)
    return True


def stop_workers_command(store, srv):
    """The heartbeat reply asks the agent to stop workers while the server is isolated."""
    last_iso = store.one("SELECT MAX(ts) t FROM events WHERE name='abuse.isolated' AND props LIKE ?", f'%"server": "{srv["id"]}"%')["t"]
    last_lift = store.one("SELECT MAX(ts) t FROM events WHERE name='abuse.lifted' AND props LIKE ?", f'%"server": "{srv["id"]}"%')["t"]
    return bool(last_iso and (not last_lift or last_lift < last_iso))
