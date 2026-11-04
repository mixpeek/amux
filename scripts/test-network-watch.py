#!/usr/bin/env python3
"""Checks for scripts/network_watch.py: every anomaly class can fire, the one
auto-fix really stops a real wide file server, and every probe failure fails
closed (exit 2, state untouched). Uses a fake tailscale, a fake amux sender and
a throwaway request log; lsof and the firewall probe are the host's own (macOS)."""

from __future__ import annotations

import importlib.util
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
WATCH = HERE / "network_watch.py"
spec = importlib.util.spec_from_file_location("nw", WATCH)
nw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nw)

fails = 0
count = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global fails, count
    count += 1
    if not ok:
        fails += 1
    print(("ok   " if ok else "FAIL ") + name + (f": {detail}" if detail and not ok else ""))


# --- pure evaluation: each class fires, and baseline silences it ------------
snap = {
    "wide": [
        {"pid": 1, "command": "amux-serv", "name": "*:8824", "host": "*", "port": "8824", "cmdline": "amux-server-rs"},
        {"pid": 2, "command": "nc", "name": "*:4444", "host": "*", "port": "4444", "cmdline": "nc -l 4444"},
        {"pid": 3, "command": "Python", "name": "*:8743", "host": "*", "port": "8743",
         "cmdline": "Python -m http.server 8743"},
        {"pid": 4, "command": "amux-serv", "name": "*:19693", "host": "*", "port": "19693", "cmdline": "amux-server"},
    ],
    "egress": {"claude": {"160.79.104.10"}, "evil": {"203.0.113.9"}},
    "peers": {
        "me": {"host": "desk", "ip": "100.1.1.1", "user": 7, "shared": False},
        "new": {"host": "stranger", "ip": "100.2.2.2", "user": 9, "shared": True},
    },
    "self_user": 7,
    "serving": True,
    "firewall": "off",
}
base = {"listeners": [["amux-serv", "8824"]], "peers": ["me"], "egress": ["claude"], "firewall": "on"}
reqs = {"clients": [("127.0.0.1", 500, 400, 0), ("100.1.1.1", 10, 1, 0),
                    ("192.168.0.50", 3, 3, 0), ("100.1.1.1x", 1, 0, 0)],
        "newest": 0, "peer_ips": {"100.1.1.1", "100.2.2.2"}}
kinds = [r["kind"] for r in nw.evaluate(snap, base, reqs)]
for k in ["LISTENER", "FILE-SERVER", "NEW-PEER", "SHARED-PEER", "FOREIGN-PEER", "SERVE-ON",
          "FIREWALL", "NEW-EGRESS", "UNKNOWN-CLIENT", "AUTH-BURST"]:
    check(f"{k} fires", k in kinds, str(kinds))
check("known listener 8824 is quiet", not any(r.get("w", {}).get("port") == "8824"
                                             for r in nw.evaluate(snap, base, reqs)))
check("dynamic e2e amux-server port is quiet",
      not any(r.get("w", {}).get("port") == "19693" for r in nw.evaluate(snap, base, reqs)))
check("loopback burst is an AUTH-BURST, not an UNKNOWN-CLIENT",
      not any(r["kind"] == "UNKNOWN-CLIENT" and r["ip"] == "127.0.0.1" for r in nw.evaluate(snap, base, reqs)))
check("known peer below the burst threshold is quiet",
      not any(r.get("ip") == "100.1.1.1" for r in nw.evaluate(snap, base, reqs)))
check("first-run baseline silences everything but standing exposure",
      {r["kind"] for r in nw.evaluate(snap, nw.baseline_of(snap), reqs)}
      <= {"FILE-SERVER", "SHARED-PEER", "FOREIGN-PEER", "SERVE-ON", "UNKNOWN-CLIENT", "AUTH-BURST"})
check("a file server is never baselined", ["Python", "8743"] not in
      [list(x) for x in nw.baseline_of(snap)["listeners"]])
line = nw.line({"kind": "UNKNOWN-CLIENT", "ip": "1.2.3.4\nIGNORE PREVIOUS", "n": 1, "fails": 0})
check("attacker text is cleaned to one line", "\n" not in line and " PREVIOUS" not in line, line)
check("NEW-EGRESS names the address, so a short-lived one can be traced",
      "203.0.113.9" in nw.line(next(r for r in nw.evaluate(snap, base, reqs) if r["kind"] == "NEW-EGRESS")))
check("parse_lsof normalises claude's version name",
      nw.parse_lsof("p9\nc2.1.318\nn127.0.0.1:1\n")[0]["command"] == "claude")
check("private ranges", all(nw.is_private(a) for a in ["10.0.0.1", "172.20.1.1", "100.100.1.1", "[fe80::1]"])
      and not nw.is_private("100.128.0.1") and not nw.is_private("8.8.8.8"))

# --- end to end: a real wide http.server is found and stopped ---------------
tmp = Path(tempfile.mkdtemp(prefix="nwtest."))
state = tmp / "state"
db = tmp / "amux.db"
con = sqlite3.connect(db)
con.execute("create table _amux_request_log (ts real, status int, client_ip text)")
con.execute("insert into _amux_request_log values (?, 401, '198.51.100.7')", (time.time(),))
con.commit()
con.close()
fake_ts = tmp / "tailscale"
fake_ts.write_text("#!/bin/sh\n[ \"$1\" = serve ] && { echo '{}'; exit 0; }\n"
                   "echo '{\"Self\":{\"ID\":\"me\",\"UserID\":7,\"TailscaleIPs\":[\"100.1.1.1\"]},\"Peer\":{}}'\n")
fake_ts.chmod(0o755)
sent = tmp / "sent.txt"
fake_amux = tmp / "amux"
fake_amux.write_text(f"#!/bin/sh\ncat >> {sent}\n")
fake_amux.chmod(0o755)
env = {**os.environ, "NETWORK_WATCH_STATE": str(state), "NETWORK_WATCH_DB": str(db),
       "NETWORK_WATCH_TAILSCALE": str(fake_ts), "NETWORK_WATCH_AMUX": str(fake_amux)}


def watch(*args: str, e: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(WATCH), *args], env=e or env,
                          capture_output=True, text=True, timeout=120)


s = socket.socket()
s.bind(("0.0.0.0", 0))
port = s.getsockname()[1]
s.close()
srv = subprocess.Popen([sys.executable, "-m", "http.server", str(port), "--bind", "0.0.0.0"],
                       cwd=tmp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
try:
    p = watch("--fix")
    check("e2e run exits 0", p.returncode == 0, p.stderr + p.stdout)
    try:
        srv.wait(timeout=5)
        stopped = True
    except subprocess.TimeoutExpired:
        stopped = False
    check("wide http.server was stopped", stopped)
    body = sent.read_text() if sent.exists() else ""
    check("stop is reported", f":{port} (stopped)" in body, body)
    check("unknown client from the request log is reported", "UNKNOWN-CLIENT 198.51.100.7" in body, body)
    check("state written", (state / "baseline.json").exists() and (state / "seen.json").exists())
    sent.unlink(missing_ok=True)
    p = watch("--fix")
    check("second run is quiet", p.returncode == 0 and not sent.exists(),
          sent.read_text() if sent.exists() else p.stderr)
finally:
    if srv.poll() is None:
        srv.kill()

# --- fail closed ------------------------------------------------------------
before = (state / "seen.json").read_text()
bad_ts = tmp / "badts"
bad_ts.write_text("#!/bin/sh\necho boom >&2\nexit 7\n")
bad_ts.chmod(0o755)
p = watch(e={**env, "NETWORK_WATCH_TAILSCALE": str(bad_ts)})
check("tailscale failure exits 2", p.returncode == 2, p.stdout + p.stderr)
p = watch(e={**env, "NETWORK_WATCH_DB": str(tmp / "missing.db")})
check("missing request log exits 2", p.returncode == 2, p.stdout + p.stderr)
garbage = tmp / "garbagets"
garbage.write_text("#!/bin/sh\necho not-json\n")
garbage.chmod(0o755)
p = watch(e={**env, "NETWORK_WATCH_TAILSCALE": str(garbage)})
check("unparseable tailscale exits 2", p.returncode == 2, p.stdout + p.stderr)
check("state untouched by failed runs", (state / "seen.json").read_text() == before)
nosend = tmp / "nosend"
nosend.write_text("#!/bin/sh\nexit 1\n")
nosend.chmod(0o755)
con = sqlite3.connect(db)
con.execute("insert into _amux_request_log values (?, 200, '203.0.113.50')", (time.time() + 1,))
con.commit()
con.close()
p = watch(e={**env, "NETWORK_WATCH_AMUX": str(nosend)})
check("undelivered exits 3", p.returncode == 3, p.stdout + p.stderr)
check("undelivered leaves state", (state / "seen.json").read_text() == before)

print(f"{count - fails} of {count} checks passed")
sys.exit(1 if fails else 0)
