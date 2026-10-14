#!/usr/bin/env python3
"""Support queue: written to ~/business/data/support.json every 5 minutes; the worker is woken only
when a request is waiting, so an empty queue costs nothing (plan M8, budget 4.7)."""
import json, os, subprocess
import _common as c

URL = c.env("CONTROL_URL") or "https://cloud.amux.io"
tok = c.env("CONTROL_ADMIN_TOKEN")
if not tok:
    c.write("support", why_unmeasured="CONTROL_ADMIN_TOKEN unset")
else:
    try:
        doc = c.write("support", c.fetch(f"{URL}/api/admin/support", {"Authorization": f"Bearer {tok}"}))
    except Exception as e:
        c.write("support", why_unmeasured=f"{type(e).__name__}: {e}"[:300])
        raise SystemExit(0)
    waiting = (doc.get("data") or {}).get("open") or []
    seen_file = os.path.join(c.DATA, ".support_seen")
    seen = set(open(seen_file).read().split()) if os.path.exists(seen_file) else set()
    new = [r for r in waiting if r["id"] not in seen]
    if new:
        prompt = (f"{len(new)} support request(s) are waiting in ~/business/data/support.json. For each: read the message "
                  "and diagnostics, and if you know the answer, POST it to {URL}/api/admin/support/draft as "
                  '{"id": ..., "text": ...}. Plain words, no em dashes. Never promise refunds or changes you cannot make; '
                  "if you do not know, say so in the draft and file a card.").replace("{URL}", URL)
        subprocess.run(["amux", "send", "amux-business", "--stdin"], input=prompt, text=True, check=False)
        with open(seen_file, "a") as f:
            f.write("".join(r["id"] + "\n" for r in new))
