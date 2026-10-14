"""Shared helpers for the Operator's collectors: fetch JSON, write ~/business/data/<name>.json atomically.
Each file records whether the measurement ran (measured, why_unmeasured) so an empty source is never read
as a quiet one."""
import json, os, time, urllib.request

DATA = os.path.expanduser(os.environ.get("BUSINESS_DATA", "~/business/data"))


def env(name):
    v = os.environ.get(name)
    if not v and os.path.exists(os.path.expanduser("~/.amux/server.env")):
        for line in open(os.path.expanduser("~/.amux/server.env")):
            if line.startswith(name + "="):
                v = line.split("=", 1)[1].strip().strip('"')
    return v


def fetch(url, headers=None, data=None, timeout=30):
    req = urllib.request.Request(url, data=json.dumps(data).encode() if data is not None else None,
                                 headers=dict({"User-Agent": "amux-business/1", "Content-Type": "application/json"}, **(headers or {})))
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def write(name, payload=None, why_unmeasured=None):
    os.makedirs(DATA, exist_ok=True)
    doc = {"collected_at": int(time.time()), "measured": why_unmeasured is None}
    if why_unmeasured:
        doc["why_unmeasured"] = why_unmeasured
    if payload is not None:
        doc["data"] = payload
    tmp = os.path.join(DATA, f".{name}.json.tmp")
    with open(tmp, "w") as f:
        json.dump(doc, f, indent=1, sort_keys=True)
    os.replace(tmp, os.path.join(DATA, f"{name}.json"))
    return doc


def run(name, fn):
    try:
        write(name, fn())
    except Exception as e:                       # the file says it did not measure; the next run tries again
        write(name, why_unmeasured=f"{type(e).__name__}: {e}"[:300])
