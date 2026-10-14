#!/usr/bin/env python3
"""Hetzner with a read-only token: fleet size and the plan prices against cost (the M13 guard's input)."""
import _common as c

tok = c.env("HCLOUD_READ_TOKEN")


def collect():
    h = {"Authorization": f"Bearer {tok}"}
    servers = c.fetch("https://api.hetzner.cloud/v1/servers?label_selector=amux-cloud%3D1&per_page=50", h)
    pricing = c.fetch("https://api.hetzner.cloud/v1/pricing", h)["pricing"]
    prices = {st["name"]: {p["location"]: float(p["price_monthly"]["net"]) for p in st["prices"]}
              for st in pricing["server_types"] if st["name"] in ("cx23", "cx33", "cx43", "cx53")}
    return {"labelled_servers": servers["meta"]["pagination"]["total_entries"], "prices": prices}


if not tok:
    c.write("hetzner", why_unmeasured="HCLOUD_READ_TOKEN unset")
else:
    c.run("hetzner", collect)
