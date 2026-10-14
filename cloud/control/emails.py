"""Transactional email for amux Cloud (docs/design/cloud-self-service.md, 8.1 emails 1-7).

Templates are plain text with a blank line between every block, so mail clients
that reflow single newlines still show the structure. The sender drains the
outbox over SMTP when SMTP_URL is set (smtp://user:pass@host:587, STARTTLS).
Without it mail stays queued and /health reports email as not configured.
"""
import json, os, smtplib, time, urllib.parse
from email.message import EmailMessage

FROM = os.environ.get("CONTROL_MAIL_FROM", "amux Cloud <hello@amux.io>")
BASE = os.environ.get("CONTROL_PUBLIC_URL", "https://cloud.amux.io")


def _date(ts):
    return time.strftime("%B %-d, %Y", time.gmtime(ts)) if ts else "soon"


def render(template, p):
    slug = p.get("slug", "")
    host = f"https://{slug}.amux.cloud" if slug else BASE
    T = {
        "being_built": ("Your amux server is being built",
                        f"Thanks for ordering amux Cloud.\n\nYour server, {host}, is being built now. It is usually ready "
                        f"in about three minutes.\n\nWatch it here: {BASE}/progress\n\nConfirm this email address so you "
                        f"can sign in from other devices and invite teammates: {BASE}/verify"),
        "provision_failed": ("We could not build your amux server",
                             "Your server did not come up within 15 minutes, so we stopped and removed it.\n\n" +
                             ("Your payment has been refunded." if p.get("refunded") else
                              "Your refund is being processed and will arrive within 7 days.") +
                             f"\n\nYou can try again at any time: {BASE}/start"),
        "payment_failed": ("Your amux Cloud payment failed",
                           f"We could not charge your card for amux Cloud.\n\nYour server keeps running for 14 days while "
                           f"we retry. Update your card here: {p.get('url') or BASE + '/billing'}"),
        "server_will_be_snapshotted": (f"Your amux server will be saved and turned off on {_date(p.get('when'))}",
                                       f"Your subscription has ended, so on {_date(p.get('when'))} we will save a "
                                       f"snapshot of {host} and turn the server off.\n\nThe snapshot is kept for 30 days, "
                                       f"and you can restore it in minutes by resubscribing: {BASE}/billing\n\n"
                                       f"Download everything now if you want a copy: {BASE}/export"),
        "server_snapshotted": ("Your amux server was saved and turned off",
                               f"We saved a snapshot of {host} and turned the server off.\n\nIt can be restored until "
                               f"{_date(p.get('snapshot_expires'))}: {BASE}/billing"),
        "deleted": ("Your amux Cloud data was deleted",
                    "Every item below has been deleted:\n\n" + "\n\n".join(f"- {x}" for x in p.get("items", []))),
        "invited": (f"You were invited to an amux server",
                    f"{p.get('inviter') or 'A teammate'} invited you to their amux server.\n\nAccept the invitation: "
                    f"{p.get('invite_url')}\n\nYou will sign in with this email address."),
        "verify": ("Confirm your email for amux Cloud",
                   f"Confirm this address to sign in from other devices and invite teammates:\n\n{p.get('url')}"),
        "abuse_notice": (f"Action on your amux server {slug}",
                         (f"We stopped outbound traffic from {host} and paused its workers because of: {p.get('why')}."
                          if p.get("isolated") else f"Your server {host} showed this: {p.get('why')}. Nothing has been changed.") +
                         "\n\nYour data is intact. If this is a mistake, reply to this email and tell us what the server was doing; "
                         "we lift the block when the cause is clear."),
        "setup_model": ("One step left: connect a model to your amux server",
                        f"Your server {host} is ready, but no model is connected yet.\n\nOpen it and choose Sign in with "
                        f"Claude, ChatGPT or Google, or paste an API key: {BASE}/"),
        "setup_first_task": ("Give your amux server its first task",
                             f"Your server {host} has a model connected but has not run anything yet.\n\nOpen it and pick a "
                             f"template, such as a nightly digest or work on a repository: {BASE}/"),
        "inactive": ("Your amux server has been quiet for a week",
                     f"Nothing has run on {host} for a week. Customers who keep amux busy most often start with a nightly "
                     f"schedule or a worker on one of their repositories.\n\nOpen your server: {BASE}/"),
        "upgrade_suggestion": ("Your amux server is running at capacity",
                               f"{host} has been above 80% CPU or memory for three days. A larger plan removes that limit; "
                               f"change it any time in billing: {BASE}/billing"),
        "upgrade_scheduled": ("Your amux server is being upgraded",
                              f"{host} moves to the {p.get('plan')} plan in the next few minutes. It will be offline for "
                              "about a minute while it restarts on the larger server."),
        "downgrade_manual": ("About your plan change",
                             f"Moving {host} to the smaller {p.get('plan')} plan needs a move to a server with a smaller "
                             "disk, which is not automatic yet. Your server keeps running on its current size until then, "
                             "and you are billed for the plan you chose."),
        "disk_full": ("Your amux server is almost out of disk",
                      f"{host} is over 95% full after we cleared logs and caches, so the rest is your files and "
                      f"repositories. Free some space, or move to a larger plan: {BASE}/billing"),
        "support_reply": ("Re: your amux Cloud request", p.get("text") or ""),
        "weekly_report": ("amux Cloud this week", p.get("text") or ""),
        "waitlist_open": ("amux Cloud is open",
                          f"You asked us to tell you when amux Cloud opened. It has.\n\nGet your server: {BASE}/start"),
    }
    if template not in T:
        raise KeyError(f"unknown email template {template}")
    subject, body = T[template]
    return subject, body + "\n\n-- \namux Cloud\n"


def configured():
    return bool(os.environ.get("SMTP_URL"))


def drain(store, limit=20):
    """Send queued mail. Returns (sent, failed). Never raises; failures are recorded on the row."""
    url = os.environ.get("SMTP_URL")
    if not url:
        return 0, 0
    u = urllib.parse.urlparse(url)
    sent = failed = 0
    rows = store.all("SELECT * FROM outbox WHERE sent_at IS NULL AND (error IS NULL OR ts < ?) ORDER BY id LIMIT ?",
                     time.time() - 600, limit)
    if not rows:
        return 0, 0
    try:
        s = smtplib.SMTP(u.hostname, u.port or 587, timeout=30)
        s.starttls()
        if u.username:
            s.login(urllib.parse.unquote(u.username), urllib.parse.unquote(u.password or ""))
    except Exception as e:
        store.event("email.connect_failed", error=str(e)[:200])
        return 0, len(rows)
    with s:
        for r in rows:
            try:
                subject, body = render(r["template"], json.loads(r["params"]))
                m = EmailMessage()
                m["From"], m["To"], m["Subject"] = FROM, r["to_addr"], subject
                m.set_content(body)
                s.send_message(m)
                store.run("UPDATE outbox SET sent_at=?, error=NULL WHERE id=?", time.time(), r["id"])
                sent += 1
            except Exception as e:
                store.run("UPDATE outbox SET error=? WHERE id=?", str(e)[:300], r["id"])
                failed += 1
    return sent, failed
