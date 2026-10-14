#!/usr/bin/env python3
"""Control plane: the same lines the weekly report prints, every 15 minutes."""
import _common as c

URL = c.env("CONTROL_URL") or "https://cloud.amux.io"
tok = c.env("CONTROL_ADMIN_TOKEN")
if not tok:
    c.write("control", why_unmeasured="CONTROL_ADMIN_TOKEN unset")
else:
    c.run("control", lambda: c.fetch(f"{URL}/api/admin/report?days=7", {"Authorization": f"Bearer {tok}"}))
